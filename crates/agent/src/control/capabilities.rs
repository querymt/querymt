use serde::{Deserialize, Serialize};
use typeshare::typeshare;

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlAgentInfo {
    pub id: String,
    pub display_name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlTransportInfo {
    pub acp: bool,
    pub stdio: bool,
    pub websocket: bool,
    pub mesh: bool,
    pub mesh_transport: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlFeatureInfo {
    pub mesh: bool,
    pub mesh_invites: bool,
    pub remote_sessions: bool,
    pub schedules: bool,
    pub remote_schedules: bool,
    pub profiles: bool,
    pub auth: bool,
    pub models: bool,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitiesInfo {
    pub querymt_control_version: u32,
    pub agent: ControlAgentInfo,
    pub transport: ControlTransportInfo,
    pub features: ControlFeatureInfo,
    pub methods: Vec<String>,
    pub notifications: Vec<String>,
}

pub fn get_capabilities(agent: &crate::LocalAgentHandle) -> CapabilitiesInfo {
    let mesh = agent.mesh();
    let mesh_enabled = mesh.is_some();
    let mesh_transport = match mesh.as_ref().map(|mesh| mesh.transport_mode()) {
        #[cfg(feature = "remote")]
        Some(crate::agent::remote::MeshTransportMode::Lan) => "lan".to_string(),
        #[cfg(feature = "remote")]
        Some(crate::agent::remote::MeshTransportMode::Iroh) => "iroh".to_string(),
        #[cfg(feature = "remote")]
        Some(crate::agent::remote::MeshTransportMode::Composite) => "multi".to_string(),
        None => "none".to_string(),
    };
    let mesh_invites = {
        #[cfg(feature = "remote")]
        {
            mesh.as_ref().is_some_and(|mesh| {
                mesh.invite_store().is_some() && mesh.is_iroh_transport_internal()
            })
        }
        #[cfg(not(feature = "remote"))]
        {
            false
        }
    };

    let mut methods = vec![
        "querymt/capabilities".to_string(),
        "querymt/models".to_string(),
        "querymt/refreshModels".to_string(),
        "querymt/modelInfo".to_string(),
        "querymt/auth/status".to_string(),
        "querymt/auth/start".to_string(),
        "querymt/auth/complete".to_string(),
        "querymt/auth/logout".to_string(),
        "querymt/schedules/create".to_string(),
        "querymt/schedules/list".to_string(),
        "querymt/schedules/get".to_string(),
        "querymt/schedules/pause".to_string(),
        "querymt/schedules/resume".to_string(),
        "querymt/schedules/trigger".to_string(),
        "querymt/schedules/delete".to_string(),
    ];

    #[cfg(feature = "remote")]
    {
        methods.extend([
            "querymt/mesh/status".to_string(),
            "querymt/mesh/join".to_string(),
            "querymt/mesh/nodes".to_string(),
            "querymt/mesh/createInvite".to_string(),
            "querymt/mesh/listInvites".to_string(),
            "querymt/mesh/revokeInvite".to_string(),
            "querymt/remote/sessions".to_string(),
            "querymt/remote/createSession".to_string(),
            "querymt/remote/attachSession".to_string(),
            "querymt/remote/dismissSession".to_string(),
        ]);
    }

    CapabilitiesInfo {
        querymt_control_version: 1,
        agent: ControlAgentInfo {
            id: "local-agent".to_string(),
            display_name: "QueryMT Agent".to_string(),
            kind: if mesh_enabled {
                "mesh".to_string()
            } else {
                "local".to_string()
            },
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        },
        transport: ControlTransportInfo {
            acp: true,
            stdio: true,
            websocket: cfg!(feature = "api"),
            mesh: mesh_enabled,
            mesh_transport,
        },
        features: ControlFeatureInfo {
            mesh: cfg!(feature = "remote") && mesh_enabled,
            mesh_invites,
            remote_sessions: cfg!(feature = "remote"),
            schedules: true,
            remote_schedules: cfg!(feature = "remote") && mesh_enabled,
            profiles: agent.profiles().is_some(),
            auth: true,
            models: true,
        },
        methods,
        notifications: vec![
            "querymt/mesh/nodesChanged".to_string(),
            "querymt/mesh/joined".to_string(),
            "querymt/mesh/peerExpired".to_string(),
            "querymt/models/changed".to_string(),
            "querymt/schedules/changed".to_string(),
        ],
    }
}
