use super::{
    adapters::HTTPFactoryAdapter, FactoryCtor, HTTPFactoryCtor, HTTPLLMProviderFactory,
    LLMProviderFactory, ProviderRegistry,
};
use libloading::Library;
use once_cell::sync::{Lazy, OnceCell};
use std::sync::{Arc, RwLock};
use std::{collections::HashMap, fs, path::Path, path::PathBuf};

pub struct NativeProviderRegistry {
    plugin_folder: PathBuf,
    factories: RwLock<HashMap<String, Arc<dyn LLMProviderFactory>>>,
}

impl NativeProviderRegistry {
    pub fn new<P: AsRef<Path>>(plugin_folder: P) -> Self {
        let folder: &Path = plugin_folder.as_ref();
        let initial_map = Self::scan_folder(folder);
        NativeProviderRegistry {
            plugin_folder: folder.to_path_buf(),
            factories: RwLock::new(initial_map),
        }
    }

    fn scan_folder(folder: &Path) -> HashMap<String, Arc<dyn LLMProviderFactory>> {
        let mut m = HashMap::new();

        let native_dir = PathBuf::from(folder);
        for entry in fs::read_dir(&native_dir).unwrap().filter_map(Result::ok) {
            let path = entry.path();
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if matches!(ext, "so" | "dll" | "dylib") {
                    // Attempt to load the library

                    let lib = unsafe {
                        match Library::new(&path) {
                            Ok(l) => l,
                            Err(e) => {
                                eprintln!("warning: failed to load {}: {}", path.display(), e);
                                continue;
                            }
                        }
                    };

                    let factory: Arc<dyn LLMProviderFactory> = unsafe {
                        if let Ok(async_ctor) = lib.get::<FactoryCtor>(b"plugin_factory") {
                            let raw = async_ctor();
                            if raw.is_null() {
                                eprintln!(
                                    "warning: plugin_factory returned null in {}",
                                    path.display()
                                );
                                continue;
                            }
                            Arc::from_raw(raw)
                        } else if let Ok(sync_ctor) =
                            lib.get::<HTTPFactoryCtor>(b"plugin_http_factory")
                        {
                            let raw: *mut dyn HTTPLLMProviderFactory = sync_ctor();
                            if raw.is_null() {
                                eprintln!(
                                    "warning: plugin_http_factory returned null in {}",
                                    path.display()
                                );
                                continue;
                            }
                            let sync_fact: Box<dyn HTTPLLMProviderFactory> = Box::from_raw(raw);
                            let async_fact = HTTPFactoryAdapter::new(Arc::from(sync_fact));
                            Arc::new(async_fact)
                        } else {
                            eprintln!(
                                "warning: no plugin_factory or plugin_http_factory in {}",
                                path.display()
                            );
                            continue;
                        }
                    };

                    let name = factory.name().to_string();
                    m.insert(name, factory);
                    std::mem::forget(lib);
                }
            }
        }
        m
    }

    pub fn reload(&self) {
        let new_map = Self::scan_folder(&self.plugin_folder);
        let mut write_guard = self.factories.write().unwrap();
        *write_guard = new_map;
    }
}

impl ProviderRegistry for NativeProviderRegistry {
    fn get(&self, provider: &str) -> Option<Arc<dyn LLMProviderFactory>> {
        self.factories.read().unwrap().get(provider).cloned()
    }

    fn list(&self) -> Vec<Arc<dyn LLMProviderFactory>> {
        self.factories.read().unwrap().values().cloned().collect()
    }
}

static NATIVE_PLUGIN_FOLDER: OnceCell<PathBuf> = OnceCell::new();

pub fn set_plugin_folder<P: Into<PathBuf>>(path: P) {
    NATIVE_PLUGIN_FOLDER
        .set(path.into())
        .expect("plugin folder already set");
}

pub static NATIVE_REGISTRY: Lazy<NativeProviderRegistry> = Lazy::new(|| {
    if let Some(override_path) = NATIVE_PLUGIN_FOLDER.get().cloned() {
        return NativeProviderRegistry::new(override_path);
    } else {
        panic!("NATIVE_PLUGIN_FOLDER is not set!");
    }
});
