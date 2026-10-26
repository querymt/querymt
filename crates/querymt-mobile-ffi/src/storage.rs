use keyring_core::{
    Credential, CredentialPersistence, CredentialStore, Entry, Error as KeyringError,
    api::{CredentialApi, CredentialStoreApi},
};
use std::any::Any;
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

const STORAGE_ABI_VERSION: u32 = 1;
const STORAGE_OK: i32 = 0;
const STORAGE_INVALID: i32 = 1;
const STORAGE_NOT_FOUND: i32 = 2;
const STORAGE_BACKEND_ERROR: i32 = 3;
const STORAGE_BUFFER_TOO_SMALL: i32 = 8;
const MAX_SERVICE_KEY_BYTES: usize = 256;
const MAX_SECRET_VALUE_BYTES: usize = 1024 * 1024;
const QUERYMT_SERVICE: &str = "querymt";

pub type StorageGetFn = unsafe extern "C" fn(
    user_data: *mut std::ffi::c_void,
    service: *const u8,
    service_len: usize,
    key: *const u8,
    key_len: usize,
    out: *mut u8,
    out_cap: usize,
    out_len: *mut usize,
) -> i32;
pub type StorageSetFn = unsafe extern "C" fn(
    user_data: *mut std::ffi::c_void,
    service: *const u8,
    service_len: usize,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
) -> i32;
pub type StorageRemoveFn = unsafe extern "C" fn(
    user_data: *mut std::ffi::c_void,
    service: *const u8,
    service_len: usize,
    key: *const u8,
    key_len: usize,
) -> i32;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FfiStorageCallbacks {
    pub abi_version: u32,
    pub struct_size: usize,
    pub user_data: *mut std::ffi::c_void,
    pub get: Option<StorageGetFn>,
    pub set: Option<StorageSetFn>,
    pub remove: Option<StorageRemoveFn>,
}

#[derive(Clone, Copy)]
struct RegisteredCallbacks {
    user_data: usize,
    get: StorageGetFn,
    set: StorageSetFn,
    remove: StorageRemoveFn,
}

#[derive(Default)]
struct Lifecycle {
    callbacks: Option<RegisteredCallbacks>,
    generation: u64,
    active_callbacks: usize,
    init_reservations: usize,
    shutdowns_in_flight: usize,
    live_agents: usize,
    runtime_active: bool,
}

#[derive(Default)]
struct Registry {
    lifecycle: Mutex<Lifecycle>,
}

static REGISTRY: once_cell::sync::Lazy<Arc<Registry>> =
    once_cell::sync::Lazy::new(|| Arc::new(Registry::default()));
#[cfg(target_os = "android")]
static ANDROID_STORE_INSTALLED: once_cell::sync::OnceCell<()> = once_cell::sync::OnceCell::new();

fn lock_lifecycle() -> std::sync::MutexGuard<'static, Lifecycle> {
    REGISTRY
        .lifecycle
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn busy(lifecycle: &Lifecycle) -> bool {
    lifecycle.active_callbacks != 0
        || lifecycle.init_reservations != 0
        || lifecycle.shutdowns_in_flight != 0
        || lifecycle.live_agents != 0
        || lifecycle.runtime_active
}

fn copy_callbacks(
    callbacks: FfiStorageCallbacks,
) -> Result<RegisteredCallbacks, crate::types::FfiErrorCode> {
    use crate::types::FfiErrorCode;

    if callbacks.abi_version != STORAGE_ABI_VERSION
        || callbacks.struct_size != std::mem::size_of::<FfiStorageCallbacks>()
    {
        return Err(FfiErrorCode::InvalidArgument);
    }
    let (Some(get), Some(set), Some(remove)) = (callbacks.get, callbacks.set, callbacks.remove)
    else {
        return Err(FfiErrorCode::InvalidArgument);
    };
    Ok(RegisteredCallbacks {
        user_data: callbacks.user_data as usize,
        get,
        set,
        remove,
    })
}

/// Copy and register the host table. The host retains ownership of `user_data`.
pub unsafe fn register_from_ptr(
    callbacks: *const FfiStorageCallbacks,
) -> Result<(), crate::types::FfiErrorCode> {
    use crate::types::FfiErrorCode;

    if callbacks.is_null() {
        return Err(FfiErrorCode::InvalidArgument);
    }
    let callbacks = copy_callbacks(unsafe { callbacks.read_unaligned() })?;
    let mut lifecycle = lock_lifecycle();
    if busy(&lifecycle) {
        return Err(FfiErrorCode::Busy);
    }
    if lifecycle.callbacks.is_some() {
        return Err(FfiErrorCode::AlreadyExists);
    }
    lifecycle.generation = lifecycle.generation.wrapping_add(1).max(1);
    lifecycle.callbacks = Some(callbacks);
    Ok(())
}

pub fn unregister() -> Result<(), crate::types::FfiErrorCode> {
    use crate::types::FfiErrorCode;

    let mut lifecycle = lock_lifecycle();
    if busy(&lifecycle) {
        return Err(FfiErrorCode::Busy);
    }
    if lifecycle.callbacks.take().is_none() {
        return Err(FfiErrorCode::InvalidState);
    }
    Ok(())
}

pub struct InitReservation;

impl Drop for InitReservation {
    fn drop(&mut self) {
        let mut lifecycle = lock_lifecycle();
        lifecycle.init_reservations = lifecycle.init_reservations.saturating_sub(1);
    }
}

pub struct ShutdownReservation;

impl Drop for ShutdownReservation {
    fn drop(&mut self) {
        let mut lifecycle = lock_lifecycle();
        lifecycle.shutdowns_in_flight = lifecycle.shutdowns_in_flight.saturating_sub(1);
    }
}

pub fn reserve_init() -> Result<InitReservation, crate::types::FfiErrorCode> {
    #[cfg(target_os = "android")]
    use crate::types::FfiErrorCode;

    let mut lifecycle = lock_lifecycle();
    #[cfg(target_os = "android")]
    if lifecycle.callbacks.is_none() {
        return Err(FfiErrorCode::InvalidState);
    }
    lifecycle.init_reservations = lifecycle.init_reservations.saturating_add(1);
    Ok(InitReservation)
}

pub fn reserve_shutdown() -> ShutdownReservation {
    let mut lifecycle = lock_lifecycle();
    lifecycle.shutdowns_in_flight = lifecycle.shutdowns_in_flight.saturating_add(1);
    ShutdownReservation
}

pub fn agent_attached() {
    let mut lifecycle = lock_lifecycle();
    lifecycle.live_agents = lifecycle.live_agents.saturating_add(1);
    lifecycle.runtime_active = true;
}

pub fn agent_detached() {
    let mut lifecycle = lock_lifecycle();
    lifecycle.live_agents = lifecycle.live_agents.saturating_sub(1);
}

pub fn runtime_started() {
    lock_lifecycle().runtime_active = true;
}

pub fn runtime_stopped() {
    lock_lifecycle().runtime_active = false;
}

#[cfg(target_os = "android")]
pub fn install_android_default_store() {
    ANDROID_STORE_INSTALLED.get_or_init(|| {
        let store: Arc<keyring_core::CredentialStore> =
            Arc::new(MobileCredentialStore::new(REGISTRY.clone()));
        querymt_utils::secret_store::install_android_default_store(store);
    });
}

#[cfg(not(target_os = "android"))]
pub fn install_android_default_store() {}

fn current_generation(registry: &Registry) -> Result<u64, KeyringError> {
    let lifecycle = registry
        .lifecycle
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if lifecycle.callbacks.is_none() {
        return Err(no_storage("no host storage callback table is registered"));
    }
    Ok(lifecycle.generation)
}

struct CallbackLease {
    registry: Arc<Registry>,
    callbacks: RegisteredCallbacks,
}

impl Drop for CallbackLease {
    fn drop(&mut self) {
        let mut lifecycle = self
            .registry
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lifecycle.active_callbacks = lifecycle.active_callbacks.saturating_sub(1);
    }
}

impl Registry {
    fn enter(self: &Arc<Self>, generation: u64) -> Result<CallbackLease, KeyringError> {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(callbacks) = lifecycle.callbacks else {
            return Err(no_storage("host storage callback table is unregistered"));
        };
        if lifecycle.generation != generation {
            return Err(no_storage(
                "credential entry belongs to a stale storage registration",
            ));
        }
        lifecycle.active_callbacks = lifecycle.active_callbacks.saturating_add(1);
        Ok(CallbackLease {
            registry: self.clone(),
            callbacks,
        })
    }
}

fn no_storage(message: &'static str) -> KeyringError {
    KeyringError::NoStorageAccess(Box::new(io::Error::other(message)))
}

fn callback_error(operation: &str, status: i32) -> KeyringError {
    if status == STORAGE_INVALID {
        KeyringError::Invalid(
            operation.to_string(),
            "host storage callback rejected the request".to_string(),
        )
    } else if status == STORAGE_BACKEND_ERROR {
        KeyringError::NoStorageAccess(Box::new(io::Error::other("host storage backend failed")))
    } else {
        KeyringError::PlatformFailure(Box::new(io::Error::other(format!(
            "host storage callback failed during {operation} (status {status})"
        ))))
    }
}

fn validate_specifier(service: &str, key: &str) -> Result<(), KeyringError> {
    if service != QUERYMT_SERVICE {
        return Err(KeyringError::Invalid(
            "service".to_string(),
            "only the querymt service is supported".to_string(),
        ));
    }
    let key_len = key.len();
    if !(1..=MAX_SERVICE_KEY_BYTES).contains(&key_len) {
        return Err(KeyringError::Invalid(
            "key".to_string(),
            "key length must be between 1 and 256 UTF-8 bytes".to_string(),
        ));
    }
    Ok(())
}

fn callback_get(
    registry: &Arc<Registry>,
    generation: u64,
    service: &str,
    key: &str,
) -> Result<Vec<u8>, KeyringError> {
    validate_specifier(service, key)?;
    let lease = registry.enter(generation)?;
    let service = service.as_bytes();
    let key = key.as_bytes();
    let user_data = lease.callbacks.user_data as *mut std::ffi::c_void;
    let mut required = 0usize;
    let query_status = unsafe {
        (lease.callbacks.get)(
            user_data,
            service.as_ptr(),
            service.len(),
            key.as_ptr(),
            key.len(),
            std::ptr::null_mut(),
            0,
            &mut required,
        )
    };
    if query_status == STORAGE_NOT_FOUND {
        return Err(KeyringError::NoEntry);
    }
    if query_status != STORAGE_OK && query_status != STORAGE_BUFFER_TOO_SMALL {
        return Err(callback_error("get", query_status));
    }
    if required > MAX_SECRET_VALUE_BYTES {
        return Err(KeyringError::Invalid(
            "value".to_string(),
            "host storage value exceeds 1 MiB".to_string(),
        ));
    }
    if query_status == STORAGE_OK && required == 0 {
        return Ok(Vec::new());
    }
    if query_status == STORAGE_BUFFER_TOO_SMALL && required == 0 {
        return Err(KeyringError::Invalid(
            "get".to_string(),
            "host returned buffer-too-small with a zero required length".to_string(),
        ));
    }

    let mut value = vec![0u8; required];
    let mut reported = 0usize;
    let mut status = unsafe {
        (lease.callbacks.get)(
            user_data,
            service.as_ptr(),
            service.len(),
            key.as_ptr(),
            key.len(),
            value.as_mut_ptr(),
            value.len(),
            &mut reported,
        )
    };
    if status == STORAGE_BUFFER_TOO_SMALL && reported > value.len() {
        if reported > MAX_SECRET_VALUE_BYTES {
            return Err(KeyringError::Invalid(
                "value".to_string(),
                "host storage value exceeds 1 MiB".to_string(),
            ));
        }
        value.resize(reported, 0);
        reported = 0;
        status = unsafe {
            (lease.callbacks.get)(
                user_data,
                service.as_ptr(),
                service.len(),
                key.as_ptr(),
                key.len(),
                value.as_mut_ptr(),
                value.len(),
                &mut reported,
            )
        };
    }
    if status == STORAGE_NOT_FOUND {
        return Err(KeyringError::NoEntry);
    }
    if status != STORAGE_OK {
        return Err(callback_error("get", status));
    }
    if reported > value.len() || reported > MAX_SECRET_VALUE_BYTES {
        return Err(KeyringError::Invalid(
            "get".to_string(),
            "host returned an invalid value length".to_string(),
        ));
    }
    value.truncate(reported);
    std::str::from_utf8(&value).map_err(|_| KeyringError::BadEncoding(value.clone()))?;
    Ok(value)
}

fn callback_set(
    registry: &Arc<Registry>,
    generation: u64,
    service: &str,
    key: &str,
    value: &[u8],
) -> Result<(), KeyringError> {
    validate_specifier(service, key)?;
    if value.len() > MAX_SECRET_VALUE_BYTES {
        return Err(KeyringError::Invalid(
            "value".to_string(),
            "value must not exceed 1 MiB".to_string(),
        ));
    }
    std::str::from_utf8(value).map_err(|_| {
        KeyringError::Invalid("value".to_string(), "value must be UTF-8".to_string())
    })?;
    let lease = registry.enter(generation)?;
    let status = unsafe {
        (lease.callbacks.set)(
            lease.callbacks.user_data as *mut std::ffi::c_void,
            service.as_bytes().as_ptr(),
            service.len(),
            key.as_bytes().as_ptr(),
            key.len(),
            value.as_ptr(),
            value.len(),
        )
    };
    if status == STORAGE_OK {
        Ok(())
    } else {
        Err(callback_error("set", status))
    }
}

fn callback_remove(
    registry: &Arc<Registry>,
    generation: u64,
    service: &str,
    key: &str,
) -> Result<(), KeyringError> {
    validate_specifier(service, key)?;
    let lease = registry.enter(generation)?;
    let status = unsafe {
        (lease.callbacks.remove)(
            lease.callbacks.user_data as *mut std::ffi::c_void,
            service.as_bytes().as_ptr(),
            service.len(),
            key.as_bytes().as_ptr(),
            key.len(),
        )
    };
    if status == STORAGE_OK {
        Ok(())
    } else {
        Err(callback_error("remove", status))
    }
}

struct MobileCredentialStore {
    registry: Arc<Registry>,
}

impl MobileCredentialStore {
    fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

impl std::fmt::Debug for MobileCredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MobileCredentialStore(ABI-v1)")
    }
}

impl CredentialStoreApi for MobileCredentialStore {
    fn vendor(&self) -> String {
        "QueryMT mobile host".to_string()
    }

    fn id(&self) -> String {
        "qmt-ffi-storage-abi-v1".to_string()
    }

    fn build(
        &self,
        service: &str,
        user: &str,
        modifiers: Option<&HashMap<&str, &str>>,
    ) -> keyring_core::Result<Entry> {
        validate_specifier(service, user)?;
        if modifiers.is_some_and(|modifiers| !modifiers.is_empty()) {
            return Err(KeyringError::NotSupportedByStore(
                "entry modifiers are not supported".to_string(),
            ));
        }
        let generation = current_generation(&self.registry)?;
        Ok(Entry::new_with_credential(Arc::new(MobileCredential {
            registry: self.registry.clone(),
            generation,
            service: service.to_string(),
            key: user.to_string(),
        })))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn persistence(&self) -> CredentialPersistence {
        CredentialPersistence::UntilDelete
    }
}

#[derive(Clone)]
struct MobileCredential {
    registry: Arc<Registry>,
    generation: u64,
    service: String,
    key: String,
}

impl CredentialApi for MobileCredential {
    fn set_secret(&self, secret: &[u8]) -> keyring_core::Result<()> {
        callback_set(
            &self.registry,
            self.generation,
            &self.service,
            &self.key,
            secret,
        )
    }

    fn get_secret(&self) -> keyring_core::Result<Vec<u8>> {
        callback_get(&self.registry, self.generation, &self.service, &self.key)
    }

    fn delete_credential(&self) -> keyring_core::Result<()> {
        callback_remove(&self.registry, self.generation, &self.service, &self.key)
    }

    fn get_credential(&self) -> keyring_core::Result<Option<Arc<Credential>>> {
        self.get_secret()?;
        Ok(Some(Arc::new(self.clone())))
    }

    fn get_specifiers(&self) -> Option<(String, String)> {
        Some((self.service.clone(), self.key.clone()))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    static TEST_LOCK: Mutex<()> = Mutex::new(());
    static TEST_VALUES: once_cell::sync::Lazy<Mutex<HashMap<(String, String), Vec<u8>>>> =
        once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));
    static SECOND_TABLE_CALLS: AtomicUsize = AtomicUsize::new(0);

    unsafe fn input_string(ptr: *const u8, len: usize) -> Option<String> {
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        std::str::from_utf8(bytes).ok().map(str::to_string)
    }

    unsafe extern "C" fn test_get(
        user_data: *mut std::ffi::c_void,
        service: *const u8,
        service_len: usize,
        key: *const u8,
        key_len: usize,
        out: *mut u8,
        out_cap: usize,
        out_len: *mut usize,
    ) -> i32 {
        if user_data as usize == 2 {
            SECOND_TABLE_CALLS.fetch_add(1, Ordering::SeqCst);
        }
        let (Some(service), Some(key)) = (unsafe { input_string(service, service_len) }, unsafe {
            input_string(key, key_len)
        }) else {
            return STORAGE_INVALID;
        };
        let value = TEST_VALUES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(service, key))
            .cloned();
        let Some(value) = value else {
            return STORAGE_NOT_FOUND;
        };
        unsafe { *out_len = value.len() };
        if out_cap < value.len() {
            return STORAGE_BUFFER_TOO_SMALL;
        }
        if !value.is_empty() {
            unsafe { std::ptr::copy_nonoverlapping(value.as_ptr(), out, value.len()) };
        }
        STORAGE_OK
    }

    unsafe extern "C" fn test_set(
        _user_data: *mut std::ffi::c_void,
        service: *const u8,
        service_len: usize,
        key: *const u8,
        key_len: usize,
        value: *const u8,
        value_len: usize,
    ) -> i32 {
        let (Some(service), Some(key)) = (unsafe { input_string(service, service_len) }, unsafe {
            input_string(key, key_len)
        }) else {
            return STORAGE_INVALID;
        };
        let value = unsafe { std::slice::from_raw_parts(value, value_len) }.to_vec();
        TEST_VALUES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((service, key), value);
        STORAGE_OK
    }

    unsafe extern "C" fn test_remove(
        _user_data: *mut std::ffi::c_void,
        service: *const u8,
        service_len: usize,
        key: *const u8,
        key_len: usize,
    ) -> i32 {
        let (Some(service), Some(key)) = (unsafe { input_string(service, service_len) }, unsafe {
            input_string(key, key_len)
        }) else {
            return STORAGE_INVALID;
        };
        TEST_VALUES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(service, key));
        STORAGE_OK
    }

    struct BlockingGet {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        blocked: AtomicBool,
    }

    unsafe extern "C" fn blocking_get(
        user_data: *mut std::ffi::c_void,
        service: *const u8,
        service_len: usize,
        key: *const u8,
        key_len: usize,
        out: *mut u8,
        out_cap: usize,
        out_len: *mut usize,
    ) -> i32 {
        let gate = unsafe { &*(user_data as *const BlockingGet) };
        if !gate.blocked.swap(true, Ordering::SeqCst) {
            let _ = gate.entered.send(());
            let _ = gate
                .release
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recv();
        }
        unsafe {
            test_get(
                std::ptr::null_mut(),
                service,
                service_len,
                key,
                key_len,
                out,
                out_cap,
                out_len,
            )
        }
    }

    fn table(get: StorageGetFn, user_data: usize) -> FfiStorageCallbacks {
        FfiStorageCallbacks {
            abi_version: STORAGE_ABI_VERSION,
            struct_size: std::mem::size_of::<FfiStorageCallbacks>(),
            user_data: user_data as *mut std::ffi::c_void,
            get: Some(get),
            set: Some(test_set),
            remove: Some(test_remove),
        }
    }

    fn set_test_default_store() -> Option<Arc<CredentialStore>> {
        let previous = keyring_core::get_default_store();
        let store: Arc<CredentialStore> = Arc::new(MobileCredentialStore::new(REGISTRY.clone()));
        keyring_core::set_default_store(store);
        previous
    }

    fn restore_default_store(previous: Option<Arc<CredentialStore>>) {
        if let Some(previous) = previous {
            keyring_core::set_default_store(previous);
        } else {
            keyring_core::unset_default_store();
        }
    }

    #[test]
    fn callback_store_roundtrips_empty_values_and_stale_entries_never_use_new_userdata() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = set_test_default_store();
        let first = table(test_get, 1);
        unsafe { register_from_ptr(&first) }.expect("register test table");
        let mut secrets = querymt_utils::secret_store::SecretStore {};

        secrets.set("ffi-empty", "").expect("store empty value");
        assert_eq!(secrets.try_get("ffi-empty").unwrap().as_deref(), Some(""));
        assert_eq!(secrets.try_get("ffi-missing").unwrap(), None);
        secrets
            .set("ffi-roundtrip", "local-test-secret")
            .expect("store value");
        assert_eq!(
            secrets.try_get("ffi-roundtrip").unwrap().as_deref(),
            Some("local-test-secret")
        );
        secrets.delete("ffi-roundtrip").expect("delete value");
        assert_eq!(secrets.try_get("ffi-roundtrip").unwrap(), None);

        let stale_entry = Entry::new(QUERYMT_SERVICE, "ffi-empty").expect("build stale entry");
        unregister().expect("unregister idle table");
        SECOND_TABLE_CALLS.store(0, Ordering::SeqCst);
        let second = table(test_get, 2);
        unsafe { register_from_ptr(&second) }.expect("register replacement table");
        assert!(stale_entry.get_password().is_err());
        assert_eq!(SECOND_TABLE_CALLS.load(Ordering::SeqCst), 0);
        assert!(
            Entry::new(QUERYMT_SERVICE, "ffi-missing")
                .expect("build fresh entry")
                .get_password()
                .is_err()
        );
        assert_eq!(SECOND_TABLE_CALLS.load(Ordering::SeqCst), 1);

        unregister().expect("unregister replacement");
        restore_default_store(previous);
        TEST_VALUES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    #[test]
    fn unregister_is_busy_until_an_arbitrary_thread_callback_returns() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = set_test_default_store();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let gate = Box::new(BlockingGet {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            blocked: AtomicBool::new(false),
        });
        let callbacks = table(blocking_get, (&*gate as *const BlockingGet) as usize);
        unsafe { register_from_ptr(&callbacks) }.expect("register blocking test table");
        let entry = Entry::new(QUERYMT_SERVICE, "ffi-blocking").expect("build callback entry");
        let read = std::thread::spawn(move || entry.get_password());
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("host callback entered on worker thread");
        assert_eq!(unregister(), Err(crate::types::FfiErrorCode::Busy));
        release_tx.send(()).expect("release host callback");
        assert!(read.join().expect("join callback operation").is_err());
        unregister().expect("unregister after callback returns");
        restore_default_store(previous);
    }
}
