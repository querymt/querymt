# QueryMT Agent - Mesh Networking

Mesh networking enables QueryMT Agent to collaborate across multiple machines, allowing sessions to be shared, delegates to run remotely, and LLM calls to be routed to specific nodes.

## Overview

Mesh networking uses the **kameo** actor framework with **libp2p** for peer-to-peer communication. This enables:

- **Cross-machine sessions**: Share sessions across multiple machines
- **Remote agents**: Access agents running on other machines
- **Distributed computation**: Run heavy tasks on specialized hardware
- **Load balancing**: Distribute work across multiple nodes

## Architecture

```
─────────────────                    ─────────────────
│   Machine A     │                    │   Machine B     │
│                 │  ──────────────  │                 │
│  ───────────  │  │   Internet   │  │  ───────────  │
│  │  Agent    │────  (libp2p)    ├────│  Agent    │  │
│  │  Session  │  │  │   Mesh       │  │  │  Session  │  │
│  ───────────  │  ──────────────  │  ───────────  │
│                 │                    │                 │
│  ───────────  │                    │  ───────────  │
│  │  GPU      │  │                    │  │  LLM      │  │
│  │  Worker   │  │                    │  │  Provider │  │
│  ───────────  │                    │  ───────────  │
─────────────────                    ─────────────────
```

## Quick Start

### Starting a Mesh Node

```bash
# Start mesh node (default: /ip4/0.0.0.0/tcp/9000)
cargo run --example coder_agent --features remote -- --mesh

# Start on custom port
cargo run --example coder_agent --features remote -- --mesh=/ip4/0.0.0.0/tcp/9001

# Start with dashboard and mesh
cargo run --example coder_agent --features "dashboard remote" -- --dashboard --mesh
```

### Connecting to a Mesh

```bash
# Connect to specific peer
cargo run --example coder_agent --features remote -- --mesh=/ip4/192.168.1.100/tcp/9000
```

## Configuration

### Basic Mesh Configuration

```toml
[mesh]
enabled = true
listen = "/ip4/0.0.0.0/tcp/9000"  # Multiaddr to listen on
discovery = "mdns"                 # "mdns" | "kademlia" | "none"
auto_fallback = false              # Allow mesh provider discovery

# Explicit peers to connect to
[[mesh.peers]]
name = "dev-gpu"
addr = "/ip4/192.168.1.100/tcp/9000"

# Request timeout for non-streaming calls
request_timeout_secs = 300
```

### Discovery Methods

#### mDNS (Default)

Automatic discovery on local network:

```toml
[mesh]
discovery = "mdns"
```

- **Pros**: Zero-config, automatic
- **Cons**: Local network only, may miss peers on different subnets

#### Kademlia DHT

Distributed discovery across the internet:

```toml
[mesh]
discovery = "kademlia"
```

- **Pros**: Cross-subnet, internet-wide
- **Cons**: Requires bootstrap nodes, more complex

#### Manual Peers

Explicit peer connections:

```toml
[mesh]
discovery = "none"

[[mesh.peers]]
name = "server1"
addr = "/ip4/192.168.1.100/tcp/9000"

[[mesh.peers]]
name = "server2"
addr = "/ip4/192.168.1.101/tcp/9000"
```

- **Pros**: Precise control, reliable
- **Cons**: Manual configuration required

## Multiaddr Format

Mesh addresses use libp2p multiaddr format:

```
/ip4/<IP>/tcp/<PORT>     # TCP over IPv4
/ip6/<IP>/tcp/<PORT>     # TCP over IPv6
/udp/<PORT>/quic         # QUIC over UDP
/p2p/<PEER_ID>           # Direct peer connection
```

### Examples

```bash
# Listen on all interfaces, port 9000
--mesh=/ip4/0.0.0.0/tcp/9000

# Listen on specific interface
--mesh=/ip4/192.168.1.100/tcp/9000

# Random port (OS-assigned)
--mesh=/ip4/0.0.0.0/tcp/0

# QUIC transport
--mesh=/udp/9000/quic
```

## Remote Agents

Define agents that run on remote mesh nodes:

```toml
# Mesh peer definition
[[mesh.peers]]
name = "gpu-server"
addr = "/ip4/192.168.1.100/tcp/9000"

# Remote agent configuration
[[remote_agents]]
id = "gpu-coder"
name = "GPU Coder"
description = "Coder running on GPU server with fast model"
peer = "gpu-server"
capabilities = ["gpu", "fast-model"]
```

### Remote Delegate

Delegates can run on remote nodes:

```toml
[[delegates]]
id = "remote-coder"
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
description = "Coder on remote GPU machine"
peer = "gpu-server"  # Routes LLM calls to remote node
tools = ["edit", "write_file", "shell"]
```

**Behavior:**
- LLM calls are routed to the remote node
- Tool execution happens locally on the planner node
- Enables "remote model, local session" pattern

## Session Management

### Creating Remote Sessions

```rust
use querymt_agent::prelude::*;

// Create session on remote node
let remote_session = agent
    .create_remote_session("gpu-server", "coder")
    .await?;

// Attach to remote session
let session = agent.attach_remote_session(remote_session).await?;

// Use session normally
let response = session.chat("Hello!").await?;
```

### Listing Remote Nodes

```rust
// List available mesh nodes
let nodes = agent.list_remote_nodes().await?;

for node in nodes {
    println!("Node: {} (peer_id: {})", node.name, node.peer_id);
}
```

### Attaching Existing Sessions

```rust
// Attach to a session running on another node
let attachment = agent
    .attach_session("gpu-server", "session-id-123")
    .await?;

let response = attachment.chat("Continue work").await?;
```

## Routing

### Routing Table

The mesh maintains a routing table that maps agents to nodes:

```rust
pub struct RoutingPolicy {
    pub agent_id: String,
    pub provider_target: RouteTarget,
    pub resolved_provider_node_id: Option<String>,
}

pub enum RouteTarget {
    Local,           // Run locally
    Peer(String),    // Run on specific peer
    Any,             // Run on any available peer
}
```

### Routing Snapshot

```rust
// Load routing snapshot
let snapshot = routing_handle.load();

// Get routing policy for an agent
if let Some(policy) = snapshot.get(&agent_id) {
    match &policy.provider_target {
        RouteTarget::Peer(peer_id) => {
            // Route LLM calls to peer
        }
        RouteTarget::Local => {
            // Run locally
        }
        RouteTarget::Any => {
            // Use any available node
        }
    }
}
```

## Use Cases

### 1. GPU-Accelerated Coding

```
Local Machine (CPU)          Remote Machine (GPU)
─────────────────          ────────────────
─────────────────        ─────────────────
│  Planner Agent  │        │  Coder Agent    │
│  (Lightweight)  │──────►│  (GPU-accelerated)│
─────────────────        ─────────────────
        │                          │
        │ LLM calls                │ Fast model inference
        │ (routed to GPU)          │
        ▼                          ▼
   ─────────              ─────────
   │  Local  │              │  GPU    │
   │  Model  │              │  Model  │
   ─────────              ─────────
```

**Configuration:**
```toml
[mesh]
enabled = true

[[mesh.peers]]
name = "gpu-server"
addr = "/ip4/192.168.1.100/tcp/9000"

[[delegates]]
id = "gpu-coder"
provider = "anthropic"
model = "claude-sonnet-4"
peer = "gpu-server"
tools = ["edit", "write_file", "shell"]
```

### 2. Distributed Team Collaboration

```
Developer A              Developer B              Developer C
─────────────            ────────────            ────────────
─────────────────     ─────────────────     ─────────────────
│  Session 1      │     │  Session 2      │     │  Session 3      │
│  (Feature A)    │────  (Feature B)    │────  (Feature C)    │
─────────────────     ─────────────────     ─────────────────
        │                       │                       │
        ──────────────────────────────────────────────
                                │
                        ───────────────
                        │   Shared      │
                        │   State       │
                        ───────────────
```

**Benefits:**
- Share session state across team members
- Collaborate on same codebase
- Real-time synchronization

### 3. Load Distribution

```
Load Balancer Node           Worker Nodes
─────────────────            ───────────
─────────────────          ───────── ─────────
│  Session Router │─────────►│ Worker 1│ │ Worker 2│
│                 │          │         │ │         │
│  - Distribute   │          │ Handle  │ │ Handle  │
│  - Monitor      │          │ Tasks   │ │ Tasks   │
─────────────────          ───────── ─────────
```

**Configuration:**
```toml
[mesh]
enabled = true
discovery = "kademlia"

[[mesh.peers]]
name = "worker1"
addr = "/ip4/10.0.0.1/tcp/9000"

[[mesh.peers]]
name = "worker2"
addr = "/ip4/10.0.0.2/tcp/9000"
```

### 4. Specialized Hardware

```
Development Machine      Specialized Nodes
─────────────────        ───────────────
─────────────────      ───────── ───────── ─────────
│  Development    │      │ GPU     │ │ TPU     │ │ FPGA    │
│  Agent          │─────►│ Worker  │ │ Worker  │ │ Worker  │
─────────────────      ───────── ───────── ─────────
```

## Security

### Peer Authentication

Mesh nodes authenticate using libp2p's built-in peer ID system:

```rust
// Each node has a unique peer ID
let peer_id = mesh.peer_id();

// Connections are authenticated
// Only known peers can connect
```

### Firewall Configuration

Required ports for mesh networking:

| Direction | Port | Protocol | Purpose |
|-----------|------|----------|---------|
| Inbound | 9000 (default) | TCP | Mesh connections |
| Outbound | Any | TCP/UDP | Peer discovery |

**Example firewall rules:**

```bash
# Allow inbound mesh connections
ufw allow 9000/tcp

# Allow outbound connections
ufw allow out 9000/tcp
```

### NAT Traversal

For nodes behind NAT:

1. **Port forwarding**: Forward mesh port to internal node
2. **UPnP**: Enable UPnP for automatic port forwarding
3. **Relay**: Use libp2p relay servers

## Monitoring

### Node Status

```rust
// Get local node info
let peer_id = mesh.peer_id();
let listen_addrs = mesh.listen_addresses();

println!("Local peer ID: {}", peer_id);
println!("Listening on: {:?}", listen_addrs);

// Get connected peers
let peers = mesh.connected_peers().await;
println!("Connected to {} peers", peers.len());
```

### Event Logging

Enable mesh logging:

```bash
# Enable libp2p logging
RUST_LOG=libp2p=info cargo run --features remote -- --mesh
```

### Metrics

Key metrics to monitor:

- **Connected peers**: Number of active connections
- **Latency**: Round-trip time to peers
- **Bandwidth**: Data transfer rates
- **Session count**: Number of sessions per node

## Troubleshooting

### Cannot Connect to Peer

**Symptoms:** Mesh node shows no connected peers

**Solutions:**
1. Check firewall allows mesh port
2. Verify peer address is correct
3. Ensure peer is running and listening
4. Check NAT/firewall configuration

### High Latency

**Symptoms:** Slow responses from remote agents

**Solutions:**
1. Check network bandwidth
2. Reduce mesh complexity (fewer peers)
3. Use closer geographic nodes
4. Increase `request_timeout_secs`

### Peer Discovery Issues

**Symptoms:** Cannot find peers automatically

**Solutions:**
1. Try explicit peer configuration
2. Check mDNS is enabled on network
3. Verify firewall allows multicast
4. Use Kademlia for cross-subnet discovery

### Session Attachment Fails

**Symptoms:** Cannot attach to remote session

**Solutions:**
1. Verify session exists on remote node
2. Check peer has correct permissions
3. Ensure mesh is properly configured
4. Review error logs for details

## Best Practices

### Network Configuration

1. **Use static IPs** for mesh nodes
2. **Configure port forwarding** for NAT environments
3. **Monitor bandwidth** usage
4. **Use dedicated ports** for mesh traffic

### Node Organization

1. **Group by function**: Separate planner and worker nodes
2. **Consider geography**: Place nodes close to users
3. **Plan for redundancy**: Multiple nodes for critical tasks
4. **Document topology**: Keep track of node roles

### Security

1. **Use strong peer IDs**: Generate unique keys
2. **Limit peer access**: Only allow known peers
3. **Monitor connections**: Watch for unauthorized access
4. **Encrypt traffic**: Use TLS where possible

## Examples

### Full Mesh Configuration

```toml
[mesh]
enabled = true
listen = "/ip4/0.0.0.0/tcp/9000"
discovery = "mdns"
auto_fallback = false
request_timeout_secs = 300

[[mesh.peers]]
name = "dev-gpu"
addr = "/ip4/192.168.1.100/tcp/9000"

[[mesh.peers]]
name = "build-server"
addr = "/ip4/192.168.1.101/tcp/9000"

[[remote_agents]]
id = "gpu-coder"
name = "GPU Coder"
description = "Coder on GPU machine"
peer = "dev-gpu"
capabilities = ["gpu"]

[[delegates]]
id = "gpu-coder-delegate"
provider = "anthropic"
model = "claude-sonnet-4"
peer = "dev-gpu"
tools = ["edit", "write_file", "shell"]
```

### Command Line Examples

```bash
# Start as mesh node
cargo run --features remote -- --mesh

# Start with specific address
cargo run --features remote -- --mesh=/ip4/0.0.0.0/tcp/9001

# Start with dashboard and mesh
cargo run --features "dashboard remote" -- --dashboard --mesh

# Connect to specific peer
cargo run --features remote -- --mesh=/ip4/192.168.1.100/tcp/9000
```

## Related Documentation

- [Delegation Guide](delegation.md) - Remote delegation
- [Configuration Guide](configuration.md) - Mesh configuration
- [API Reference](api_reference.md) - Mesh API types