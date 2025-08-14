use std::collections::HashMap;
use crate::plugin::gepa::GepaPlugin;

pub enum Plugin {
    Gepa(GepaPlugin),
}

pub struct PluginManager {
    plugins: HashMap<String, Plugin>,
}

impl PluginManager {
    pub fn new() -> Self {
        PluginManager {
            plugins: HashMap::new(),
        }
    }

    pub fn register(&mut self, name: &str, plugin: Plugin) {
        self.plugins.insert(name.to_string(), plugin);
    }

    pub fn get(&self, name: &str) -> Option<&Plugin> {
        self.plugins.get(name)
    }
}
