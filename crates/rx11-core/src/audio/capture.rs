//! The capture shapes that isolate ONE application's sound.
//!
//! The desktop mix is easy: record the sink's monitor. But the
//! per-application tier needs the sound of ONE application — and on a
//! PipeWire host, the obvious way to ask for it does not work.
//!
//! What was measured on a live PipeWire host:
//!
//! * `pw-cat --record --target={"object":"Stream/Output/Audio","node.id":N}`
//!   does NOT return that stream's audio. It returns the SINK MIX: asking
//!   for a stream that was playing silence yielded the tone another
//!   application was playing at that moment, and two different targets
//!   produced nearly identical captures.
//! * What does isolate an application: start a capture stream of our own
//!   (`pw-cat --record --target=0`) and LINK the application's output port
//!   into that stream's input port (`pw-link`). The capture then carries
//!   only the linked application's sound. The link survives the exit of
//!   the `pw-link` process, so linking is a one-shot action.
//! * The capture command must carry `--raw`: without it `pw-cat` writes a
//!   container header instead of bare s16le PCM.
//! * Ports must be addressed by NUMBER, not by name: several applications
//!   share a node name (every `paplay` instance is called "paplay"), and
//!   linking by name reaches the wrong one.
//!
//! This module owns the command shapes and the graph lookup that decides
//! which ports to link.

/// A node of the audio graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeWireNode {
    /// The node's id in the graph.
    pub id: u32,
    /// The node's name (not unique — several applications share one).
    pub name: String,
    /// The media class, e.g. `Stream/Output/Audio`.
    pub media_class: String,
    /// The process that produced the node, when the graph knows it.
    pub process_id: Option<i32>,
}

impl PipeWireNode {
    /// Whether this node is an application's output stream.
    pub fn is_application_output(&self) -> bool {
        self.media_class == "Stream/Output/Audio"
    }

    /// Whether the node belongs to the given process (directly).
    pub fn belongs_to(&self, pid: i32) -> bool {
        self.process_id == Some(pid)
    }
}

/// A port of the audio graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeWirePort {
    /// The port's id — the addressable handle; names are NOT unique.
    pub id: u32,
    /// The node this port belongs to.
    pub node_id: u32,
    /// The port's name, e.g. `output_FL` / `input_FL`.
    ///
    /// Unique WITHIN a node, but not across applications: every `paplay`
    /// instance names its node "paplay", so a link must still address the
    /// port by NUMBER. The name is what pairs channels — `output_FL` with
    /// `input_FL` — so that a stereo application stays stereo in the
    /// capture instead of collapsing onto one channel.
    pub name: String,
    /// `true` for an output port (the producer side).
    pub is_output: bool,
}

impl PipeWirePort {
    /// The port's direction as the `pw-link` argument expects it.
    pub fn direction(&self) -> &'static str {
        if self.is_output {
            "output"
        } else {
            "input"
        }
    }

    /// The channel suffix of the port's name — `FL` for `output_FL`.
    ///
    /// The suffix after the last underscore: `output_FL` and `input_FL`
    /// both yield `FL`, which is what pairs an application's output port
    /// with the capture stream's input port of the same channel.
    pub fn channel_suffix(&self) -> &str {
        match self.name.rsplit_once('_') {
            Some((_, suffix)) => suffix,
            None => &self.name,
        }
    }
}

/// The audio graph, reduced to what the capture path needs.
#[derive(Debug, Clone, Default)]
pub struct PipeWireTopology {
    pub nodes: Vec<PipeWireNode>,
    pub ports: Vec<PipeWirePort>,
}

impl PipeWireTopology {
    /// Read the audio graph from the host.
    ///
    /// `pw-dump` prints the whole graph as JSON; this parses the two
    /// object kinds the capture path needs — nodes and ports — and drops
    /// the rest. The parsing goes through `serde_json`, so the graph
    /// format is not hand-decoded.
    ///
    /// Fails when the host has no PipeWire, when `pw-dump` cannot run, or
    /// when its output is not the expected JSON — the caller then falls
    /// back to the desktop mix.
    pub fn read() -> crate::error::Result<Self> {
        let output = std::process::Command::new("pw-dump").output()?;
        if !output.status.success() {
            return Err(crate::error::Rx11Error::Protocol(format!(
                "pw-dump failed: {}",
                output.status
            )));
        }
        Self::from_json(&output.stdout)
    }

    /// Build the topology from `pw-dump`'s JSON output.
    pub fn from_json(bytes: &[u8]) -> crate::error::Result<Self> {
        let objects: Vec<serde_json::Value> = serde_json::from_slice(bytes).map_err(|e| {
            crate::error::Rx11Error::Protocol(format!("pw-dump output is not JSON: {e}"))
        })?;
        let mut topology = Self::default();
        for object in objects {
            let Some(kind) = object.get("type").and_then(|t| t.as_str()) else {
                continue;
            };
            let Some(id) = object.get("id").and_then(|i| i.as_u64()) else {
                continue;
            };
            let props = object
                .get("info")
                .and_then(|info| info.get("props"))
                .and_then(|props| props.as_object());
            match kind {
                "PipeWire:Interface:Node" => {
                    let Some(props) = props else { continue };
                    let name = string_prop(props, "node.name");
                    let media_class = string_prop(props, "media.class");
                    let process_id = props
                        .get("application.process.id")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32);
                    topology.nodes.push(PipeWireNode {
                        id: id as u32,
                        name,
                        media_class,
                        process_id,
                    });
                }
                "PipeWire:Interface:Port" => {
                    let Some(props) = props else { continue };
                    let Some(node_id) = props.get("node.id").and_then(|v| v.as_u64()) else {
                        continue;
                    };
                    // The direction is a plain string: "out" is the
                    // producer side, "in" the consumer side.
                    let is_output =
                        props.get("port.direction").and_then(|v| v.as_str()) == Some("out");
                    topology.ports.push(PipeWirePort {
                        id: id as u32,
                        node_id: node_id as u32,
                        name: string_prop(props, "port.name"),
                        is_output,
                    });
                }
                _ => {}
            }
        }
        Ok(topology)
    }

    /// The output ports of the node with this id.
    pub fn output_ports_of(&self, node_id: u32) -> Vec<&PipeWirePort> {
        self.ports
            .iter()
            .filter(|p| p.node_id == node_id && p.is_output)
            .collect()
    }

    /// The input ports of the node with this id.
    pub fn input_ports_of(&self, node_id: u32) -> Vec<&PipeWirePort> {
        self.ports
            .iter()
            .filter(|p| p.node_id == node_id && !p.is_output)
            .collect()
    }

    /// The output stream nodes produced by the given process.
    pub fn application_outputs(&self, pid: i32) -> Vec<&PipeWireNode> {
        self.nodes
            .iter()
            .filter(|n| n.is_application_output() && n.belongs_to(pid))
            .collect()
    }

    /// Every application's output stream node — the `Stream/Output/Audio`
    /// nodes that carry a process id.
    ///
    /// The process id is the process that PLAYS the sound (a browser's
    /// audio service), NOT the process that holds the X11 connection (the
    /// browser's main process). The two are different processes, so
    /// looking a node up by the connection's process id finds nothing;
    /// the caller instead takes each node's process id, resolves the
    /// application it belongs to, and compares applications.
    ///
    /// Nodes without a process id are left out: they belong to no
    /// application, so no application can claim them.
    pub fn application_output_nodes(&self) -> Vec<&PipeWireNode> {
        self.nodes
            .iter()
            .filter(|n| n.is_application_output() && n.process_id.is_some())
            .collect()
    }

    /// The node with this name — the capture stream is found by its own
    /// unique name, because a capture process carries no process id in the
    /// graph.
    pub fn node_named(&self, name: &str) -> Option<&PipeWireNode> {
        self.nodes.iter().find(|n| n.name == name)
    }

    /// Pair an application's output ports with the capture stream's input
    /// ports, channel by channel.
    ///
    /// The pairing is by channel suffix: `output_FL` with `input_FL`,
    /// `output_FR` with `input_FR`. Linking by position instead would
    /// silently swap the channels of an application whose ports are not
    /// listed in the same order.
    ///
    /// Ports whose suffix has no counterpart are left out — an
    /// application with a mono output links one channel, and a capture
    /// stream with more channels than the application simply does not use
    /// them.
    pub fn pair_channels(&self, output_node: u32, input_node: u32) -> Vec<(u32, u32)> {
        let inputs = self.input_ports_of(input_node);
        self.output_ports_of(output_node)
            .iter()
            .filter_map(|output| {
                inputs
                    .iter()
                    .find(|input| input.channel_suffix() == output.channel_suffix())
                    .map(|input| (output.id, input.id))
            })
            .collect()
    }
}

/// Read a string property from a `pw-dump` property map.
fn string_prop(props: &serde_json::Map<String, serde_json::Value>, key: &str) -> String {
    props
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// The capture command for ONE application.
///
/// The stream starts unlinked (`--target=0`) and receives the application
/// through a link; `node.name` is unique so the capture stream can be told
/// apart from the application's own node.
///
/// The sample format is `s16`, NOT `s16le`: `pw-cat` names the format
/// `s16` (see `pw-cat --list-formats`), and `s16le` — the spelling
/// `parec`/`paplay` use — makes the capture stream refuse to start with
/// "open failed: invalid argument". The byte order is the host's, so the
/// PCM is still interleaved little-endian s16.
pub fn capture_stream_command(node_name: &str) -> Vec<String> {
    vec![
        "pw-cat".to_string(),
        "--record".to_string(),
        "--target=0".to_string(),
        "--raw".to_string(),
        "--format=s16".to_string(),
        "--rate=48000".to_string(),
        "--channels=2".to_string(),
        format!("--properties=node.name={node_name}"),
        "-".to_string(),
    ]
}

/// The command that links an application's output port into the capture
/// stream's input port.
///
/// Both ends are port NUMBERS: names are ambiguous across applications.
///
/// No `-L` (linger) flag: `-L` does NOT mean "create the link" — the
/// default behaviour already creates the link, and the link SURVIVES the
/// exit of the `pw-link` process (measured: the capture node keeps
/// running and keeps carrying audio after `pw-link` exited). Adding `-L`
/// to a pair of ports that are already linked fails with "file exists".
pub fn link_command(output_port: u32, input_port: u32) -> Vec<String> {
    vec![
        "pw-link".to_string(),
        output_port.to_string(),
        input_port.to_string(),
    ]
}

/// Whether this host can capture per application at all: it needs the two
/// PipeWire tools and a `pw-dump` to read the graph.
pub fn per_app_capture_available() -> bool {
    super::program_exists("pw-cat")
        && super::program_exists("pw-link")
        && super::program_exists("pw-dump")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_command_is_unlinked_raw_and_uniquely_named() {
        let argv = capture_stream_command("rx11-cap-7");
        assert_eq!(argv.first().map(String::as_str), Some("pw-cat"));
        assert!(argv.iter().any(|a| a == "--record"));
        // Unlinked: the application arrives through a link, not a target.
        assert!(argv.iter().any(|a| a == "--target=0"));
        // Raw PCM: without --raw the output carries a container header.
        assert!(argv.iter().any(|a| a == "--raw"));
        // The format is `s16`, not `s16le`: `pw-cat` refuses `s16le`.
        assert!(argv.iter().any(|a| a == "--format=s16"));
        assert!(!argv.iter().any(|a| a == "--format=s16le"));
        assert!(argv.iter().any(|a| a == "--rate=48000"));
        assert!(argv.iter().any(|a| a == "--channels=2"));
        // The unique name is what tells the capture stream apart later.
        assert!(argv
            .iter()
            .any(|a| a == "--properties=node.name=rx11-cap-7"));
        assert_eq!(argv.last().map(String::as_str), Some("-"));
    }

    #[test]
    fn link_command_addresses_ports_by_number() {
        let argv = link_command(75, 106);
        assert_eq!(argv, vec!["pw-link", "75", "106"]);
        // Numbers, not names: several applications share a node name.
        assert!(argv
            .iter()
            .all(|a| a.chars().all(|c| c.is_ascii_digit()) || a == "pw-link"));
    }

    #[test]
    fn topology_finds_an_application_output_by_process() {
        let topology = PipeWireTopology {
            nodes: vec![
                PipeWireNode {
                    id: 78,
                    name: "paplay".into(),
                    media_class: "Stream/Output/Audio".into(),
                    process_id: Some(52075),
                },
                PipeWireNode {
                    id: 79,
                    name: "paplay".into(),
                    media_class: "Stream/Output/Audio".into(),
                    process_id: Some(52076),
                },
                PipeWireNode {
                    id: 57,
                    name: "sink".into(),
                    media_class: "Audio/Sink".into(),
                    process_id: None,
                },
            ],
            ports: vec![
                PipeWirePort {
                    id: 75,
                    node_id: 78,
                    is_output: true,
                    name: "output_FL".into(),
                },
                PipeWirePort {
                    id: 79,
                    node_id: 78,
                    is_output: true,
                    name: "output_FR".into(),
                },
                PipeWirePort {
                    id: 80,
                    node_id: 78,
                    is_output: false,
                    name: "input_FL".into(),
                },
                PipeWirePort {
                    id: 81,
                    node_id: 79,
                    is_output: true,
                    name: "output_FL".into(),
                },
            ],
        };

        let outputs = topology.application_outputs(52075);
        assert_eq!(outputs.len(), 1, "only the node of that process");
        assert_eq!(outputs[0].id, 78);

        // The ports of that node: two outputs and one input.
        let out_ports = topology.output_ports_of(78);
        assert_eq!(out_ports.len(), 2);
        assert!(out_ports.iter().all(|p| p.is_output));
        let in_ports = topology.input_ports_of(78);
        assert_eq!(in_ports.len(), 1);
        assert!(!in_ports[0].is_output);

        // The capture stream is found by its unique name, not by process.
        let capture = PipeWireNode {
            id: 90,
            name: "rx11-cap-7".into(),
            media_class: "Stream/Input/Audio".into(),
            process_id: None,
        };
        let mut topology = topology;
        topology.nodes.push(capture);
        assert_eq!(topology.node_named("rx11-cap-7").map(|n| n.id), Some(90));
        assert!(topology.node_named("paplay").is_some());
    }

    #[test]
    fn a_node_without_a_process_id_never_matches_an_application() {
        let node = PipeWireNode {
            id: 1,
            name: "capture".into(),
            media_class: "Stream/Output/Audio".into(),
            process_id: None,
        };
        assert!(!node.belongs_to(42));
    }

    #[test]
    fn application_output_nodes_carry_a_process_id() {
        // The list the capture path walks: every application's output
        // stream, each naming the process that PLAYS the sound.
        let topology = PipeWireTopology {
            nodes: vec![
                PipeWireNode {
                    id: 78,
                    name: "paplay".into(),
                    media_class: "Stream/Output/Audio".into(),
                    process_id: Some(52075),
                },
                PipeWireNode {
                    id: 79,
                    name: "paplay".into(),
                    media_class: "Stream/Output/Audio".into(),
                    process_id: Some(52076),
                },
                PipeWireNode {
                    id: 57,
                    name: "sink".into(),
                    media_class: "Audio/Sink".into(),
                    process_id: None,
                },
                PipeWireNode {
                    id: 90,
                    name: "rx11-cap-7".into(),
                    media_class: "Stream/Input/Audio".into(),
                    process_id: None,
                },
                PipeWireNode {
                    id: 91,
                    name: "orphan".into(),
                    media_class: "Stream/Output/Audio".into(),
                    process_id: None,
                },
            ],
            ports: Vec::new(),
        };

        let nodes = topology.application_output_nodes();
        assert_eq!(nodes.len(), 2, "only the output streams with a process");
        assert!(nodes.iter().all(|n| n.is_application_output()));
        assert!(nodes.iter().all(|n| n.process_id.is_some()));
        let ids: Vec<u32> = nodes.iter().map(|n| n.id).collect();
        assert!(ids.contains(&78) && ids.contains(&79));
        // The sink and our own capture stream are not application outputs.
        assert!(!ids.contains(&57) && !ids.contains(&90) && !ids.contains(&91));
    }

    #[test]
    fn channels_are_paired_by_suffix_not_by_position() {
        // An application whose ports are listed FR first must still pair
        // FL with FL and FR with FR.
        let topology = PipeWireTopology {
            nodes: Vec::new(),
            ports: vec![
                PipeWirePort {
                    id: 10,
                    node_id: 1,
                    is_output: true,
                    name: "output_FR".into(),
                },
                PipeWirePort {
                    id: 11,
                    node_id: 1,
                    is_output: true,
                    name: "output_FL".into(),
                },
                PipeWirePort {
                    id: 20,
                    node_id: 2,
                    is_output: false,
                    name: "input_FL".into(),
                },
                PipeWirePort {
                    id: 21,
                    node_id: 2,
                    is_output: false,
                    name: "input_FR".into(),
                },
            ],
        };

        let pairs = topology.pair_channels(1, 2);
        assert_eq!(pairs.len(), 2, "both channels are paired");
        assert!(
            pairs.contains(&(11, 20)),
            "output_FL pairs with input_FL: {pairs:?}"
        );
        assert!(
            pairs.contains(&(10, 21)),
            "output_FR pairs with input_FR: {pairs:?}"
        );
        // The pairing is by suffix, so the order of the ports does not
        // matter and no channel is crossed.
        for (out_port, in_port) in &pairs {
            let out_suffix = topology
                .ports
                .iter()
                .find(|p| p.id == *out_port)
                .expect("output port")
                .channel_suffix();
            let in_suffix = topology
                .ports
                .iter()
                .find(|p| p.id == *in_port)
                .expect("input port")
                .channel_suffix();
            assert_eq!(out_suffix, in_suffix, "channels must not cross");
        }
    }

    #[test]
    fn a_mono_application_pairs_only_one_channel() {
        let topology = PipeWireTopology {
            nodes: Vec::new(),
            ports: vec![
                PipeWirePort {
                    id: 10,
                    node_id: 1,
                    is_output: true,
                    name: "output_FL".into(),
                },
                PipeWirePort {
                    id: 20,
                    node_id: 2,
                    is_output: false,
                    name: "input_FL".into(),
                },
                PipeWirePort {
                    id: 21,
                    node_id: 2,
                    is_output: false,
                    name: "input_FR".into(),
                },
            ],
        };

        let pairs = topology.pair_channels(1, 2);
        assert_eq!(pairs, vec![(10, 20)], "only the FL channel exists");
    }
}
