# Phase 6A — design notes

- **The pod network moved to the standard CNI shape.** Each node runs a Linux
  bridge `rk-br0` holding the node's `/24` gateway (`.1`); every pod's veth
  host-end is enslaved to it, and the peer becomes `eth0` with a `/24` address
  and a default route via the gateway. Same-node pods now share the bridge's
  L2 segment and reach each other directly — Phase 1's `/30`-per-pod links
  couldn't. IPAM accordingly hands out `/32` pod IPs (`.2`..`.254`), not `/30`
  blocks.
- **Cross-node traffic rides a VXLAN overlay, flannel's unicast way.** One
  VXLAN device per node (`rk-vxlan0`, VNI 42, learning off) whose forwarding
  is programmed, never flooded: for each peer node the agent installs a
  **route** (`<peer /24> via <peer VTEP ip> onlink`), a static **neighbour**
  (peer VTEP ip → peer MAC), and an **FDB** entry (peer MAC → peer underlay
  address). IP forwarding ties the bridge and the tunnel together.
- **The Node object is the overlay's control plane.** An agent advertises its
  VTEP — pod CIDR (`NodeSpec.podCidr`), underlay address (`NodeStatus.address`)
  and MAC (an annotation) — on its own Node, then watches the Node list and
  programs/withdraws peer entries as nodes come and go. No extra coordination
  channel; the same list+watch reflector the scheduler and controllers use.
- **VTEP MACs are derived, not read back.** The endpoint MAC is computed from
  the underlay IP (`2a:00:<a.b.c.d>`) and set explicitly on the device. The
  first cut read the kernel's random MAC and advertised that — and a single
  read-back race meant the advertised MAC didn't match the device, so reply
  frames addressed to the advertised MAC were silently dropped and every
  cross-node connection hung. Deriving the MAC makes device == advertised by
  construction, and a peer could even compute it without the annotation.
- **The VTEP overlay IP is deterministic too**: a node's pod-`/24` network
  address (`10.42.1.0/24` → `10.42.1.0`), so peers derive the route next-hop
  from the advertised CIDR with nothing else to agree on. The route is
  `onlink` because that next-hop sits inside the destination subnet.
- **Reverse-path filtering is relaxed** (`rp_filter=0`): overlay routing is
  symmetric so strict filtering *should* pass, but some hosts' defaults drop
  the decapsulated packets as martians — cheaper to disable than to debug.
- **Simulation note**: the two "nodes" are an agent in the host netns and one
  in a `netns` (Phase 4's model), tunnelling over the `10.200.0.0/24` veth
  link between them; the demo pins each node's `nodeIp` to its link address so
  the VXLAN underlay is the shared segment, not the VM's external IP. On real
  machines the node's real address is its underlay and no pinning is needed.
- **Deferred to 6B**: Services (ClusterIP/NodePort virtual IPs load-balanced
  across pod endpoints with nftables) and the embedded DNS server resolving
  `service.namespace`. Pod-to-pod by IP is the foundation they build on.
