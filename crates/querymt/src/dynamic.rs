#[cfg(feature = "extism_host")]
use crate::plugin::extism_impl::host::ExtismLoader;
use crate::plugin::host::PluginRegistry;
#[cfg(feature = "native")]
use crate::plugin::host::native::NativeLoader;

pub trait PluginRegistryDynamicExt: Sized {
    fn register_dynamic_loaders(&mut self);
    fn with_dynamic_loaders(self) -> Self;
}

impl PluginRegistryDynamicExt for PluginRegistry {
    fn register_dynamic_loaders(&mut self) {
        #[cfg(feature = "extism_host")]
        self.register_loader(Box::new(ExtismLoader));

        #[cfg(feature = "native")]
        self.register_loader(Box::new(NativeLoader));
    }

    fn with_dynamic_loaders(mut self) -> Self {
        self.register_dynamic_loaders();
        self
    }
}
