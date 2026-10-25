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
}

/// The audio graph, reduced to what the capture path needs.
#[derive(Debug, Clone, Default)]
pub struct PipeWireTopology {
    pub nodes: Vec<PipeWireNode>,
    pub ports: Vec<PipeWirePort>,
}

impl PipeWireTopology {
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

    /// The node with this name — the capture stream is found by its own
    /// unique name, because a capture process carries no process id in the
    /// graph.
    pub fn node_named(&self, name: &str) -> Option<&PipeWireNode> {
        self.nodes.iter().find(|n| n.name == name)
    }
}

/// The capture command for ONE application.
///
/// The stream starts unlinked (`--target=0`) and receives the application
/// through a link; `node.name` is unique so the capture stream can be told
/// apart from the application's own node.
pub fn capture_stream_command(node_name: &str) -> Vec<String> {
    vec![
        "pw-cat".to_string(),
        "--record".to_string(),
        "--target=0".to_string(),
        "--raw".to_string(),
        "--format=s16le".to_string(),
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
        assert!(argv.iter().any(|a| a == "--format=s16le"));
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
                },
                PipeWirePort {
                    id: 79,
                    node_id: 78,
                    is_output: true,
                },
                PipeWirePort {
                    id: 80,
                    node_id: 78,
                    is_output: false,
                },
                PipeWirePort {
                    id: 81,
                    node_id: 79,
                    is_output: true,
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
}
