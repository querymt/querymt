use crate::{MeshScopeId, MeshTransportKind, MeshTransportMode};

/// Compose a scope-prefixed actor name.
pub fn scoped_actor_name(scope: &MeshScopeId, name: &str) -> String {
    format!("{}{}", scope.dht_prefix(), name)
}

/// Forward-compatible mapping from runtime transport mode to enabled transports.
pub fn enabled_transports_from_mode(mode: MeshTransportMode) -> Vec<MeshTransportKind> {
    match mode {
        MeshTransportMode::Lan => vec![MeshTransportKind::Lan],
        MeshTransportMode::Iroh => vec![MeshTransportKind::Iroh],
        MeshTransportMode::Composite => vec![MeshTransportKind::Lan, MeshTransportKind::Iroh],
    }
}

/// Whether a transport kind is enabled for the given runtime mode.
pub fn mode_has_transport(mode: MeshTransportMode, kind: MeshTransportKind) -> bool {
    enabled_transports_from_mode(mode).contains(&kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_actor_name_prefixes_scope() {
        let scope = MeshScopeId::Iroh {
            mesh_id: "team-a".to_string(),
        };
        assert_eq!(
            scoped_actor_name(&scope, "node_manager"),
            "scope::iroh::team-a::node_manager"
        );
    }

    #[test]
    fn enabled_transports_match_mode() {
        assert_eq!(
            enabled_transports_from_mode(MeshTransportMode::Lan),
            vec![MeshTransportKind::Lan]
        );
        assert_eq!(
            enabled_transports_from_mode(MeshTransportMode::Iroh),
            vec![MeshTransportKind::Iroh]
        );
        assert_eq!(
            enabled_transports_from_mode(MeshTransportMode::Composite),
            vec![MeshTransportKind::Lan, MeshTransportKind::Iroh]
        );
    }

    #[test]
    fn mode_has_transport_checks_membership() {
        assert!(mode_has_transport(MeshTransportMode::Composite, MeshTransportKind::Lan));
        assert!(mode_has_transport(MeshTransportMode::Composite, MeshTransportKind::Iroh));
        assert!(!mode_has_transport(MeshTransportMode::Lan, MeshTransportKind::Iroh));
    }
}
