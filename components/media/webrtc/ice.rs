//! Browser ICE server configuration converted to the URI forms accepted by `webrtcbin`.

use std::error::Error;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

/// ICE candidate gathering policy used by a peer connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IceTransportPolicy {
    /// Gather all available candidate types.
    #[default]
    All,
    /// Restrict candidates to those obtained through TURN relays.
    Relay,
}

/// One browser-style ICE server entry.
///
/// `username` and `credential` are used for TURN URLs. This type intentionally
/// does not implement `Debug` so authentication values are not logged by an
/// accidental debug print.
pub struct IceServer {
    /// Browser-form ICE server URLs, such as `stun:host:3478` or
    /// `turn:host:3478?transport=tcp`.
    pub urls: Vec<String>,
    /// TURN username shared by all TURN URLs in this entry.
    pub username: Option<String>,
    /// TURN password shared by all TURN URLs in this entry.
    pub credential: Option<String>,
}

/// ICE server values normalized for GStreamer `webrtcbin`.
///
/// `turn_servers` contains authentication-bearing URIs. The `Debug`
/// implementation reports only whether a STUN server is configured and how
/// many TURN servers are present, never the URI values.
pub struct IceConfiguration {
    /// The single STUN server supported by `webrtcbin`, if configured.
    pub stun_server: Option<String>,
    /// TURN URIs to pass separately to `webrtcbin`'s `add-turn-server` action.
    pub turn_servers: Vec<String>,
    /// Candidate policy to apply to `webrtcbin`.
    pub policy: IceTransportPolicy,
}

impl fmt::Debug for IceConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IceConfiguration")
            .field(
                "stun_server",
                &self.stun_server.as_ref().map(|_| "[configured]"),
            )
            .field(
                "turn_servers",
                &format_args!("[REDACTED; {} configured]", self.turn_servers.len()),
            )
            .field("policy", &self.policy)
            .finish()
    }
}

impl Default for IceConfiguration {
    fn default() -> Self {
        Self {
            stun_server: None,
            turn_servers: Vec::new(),
            policy: IceTransportPolicy::All,
        }
    }
}

/// A validation or conversion error for browser ICE server configuration.
///
/// Error values include only indexes and static error kinds; they never retain
/// or display an input URL, username, or credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IceConfigError {
    /// An ICE server entry contained no URLs.
    EmptyUrls { server_index: usize },
    /// A URL could not be parsed or had no host.
    InvalidUrl {
        server_index: usize,
        url_index: usize,
    },
    /// A URL scheme is not supported by this `webrtcbin` conversion.
    UnsupportedScheme {
        server_index: usize,
        url_index: usize,
    },
    /// A URL contained a path or fragment that cannot be represented.
    UnsupportedUrlComponent {
        server_index: usize,
        url_index: usize,
    },
    /// A TURN URL contained an unsupported query or transport value.
    UnsupportedTransport {
        server_index: usize,
        url_index: usize,
    },
    /// TURN credentials were absent or only one of username and credential was set.
    InvalidCredentials { server_index: usize },
    /// Legacy error retained for source compatibility; multiple STUN URLs now
    /// select the first URL instead of returning this error.
    MultipleStunServers,
    /// The configured UDP/TCP port was zero.
    InvalidPort {
        server_index: usize,
        url_index: usize,
    },
}

impl fmt::Display for IceConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyUrls { server_index } => {
                write!(f, "ICE server entry {server_index} has no URLs")
            },
            Self::InvalidUrl {
                server_index,
                url_index,
            } => write!(
                f,
                "ICE server entry {server_index}, URL {url_index} is invalid"
            ),
            Self::UnsupportedScheme {
                server_index,
                url_index,
            } => write!(
                f,
                "ICE server entry {server_index}, URL {url_index} uses an unsupported scheme"
            ),
            Self::UnsupportedUrlComponent {
                server_index,
                url_index,
            } => write!(
                f,
                "ICE server entry {server_index}, URL {url_index} contains an unsupported path or fragment"
            ),
            Self::UnsupportedTransport {
                server_index,
                url_index,
            } => write!(
                f,
                "ICE server entry {server_index}, URL {url_index} has an unsupported transport query"
            ),
            Self::InvalidCredentials { server_index } => write!(
                f,
                "ICE server entry {server_index} must provide both TURN username and credential"
            ),
            Self::MultipleStunServers => {
                f.write_str("webrtcbin supports one STUN server, but multiple were configured")
            },
            Self::InvalidPort {
                server_index,
                url_index,
            } => write!(
                f,
                "ICE server entry {server_index}, URL {url_index} has an invalid port"
            ),
        }
    }
}

impl Error for IceConfigError {}

impl IceConfiguration {
    /// Converts browser-style ICE servers into GStreamer `webrtcbin` values.
    ///
    /// TURN URLs require both username and password because `webrtcbin`
    /// validates both fields before adding a TURN server. Its `stun-server`
    /// property accepts one URI, so this method keeps the first `stun:` URL
    /// and validates every later URL before returning. `stuns:` is rejected
    /// because `webrtcbin`'s documented STUN URI is non-TLS `stun://`.
    ///
    /// # Errors
    ///
    /// Returns an error for empty URL lists, malformed URLs, unsupported
    /// schemes or URL components, unsupported TURN transports, incomplete
    /// TURN credentials, or zero ports.
    pub fn new(
        servers: Vec<IceServer>,
        policy: IceTransportPolicy,
    ) -> Result<Self, IceConfigError> {
        let mut config = Self {
            policy,
            ..Self::default()
        };

        for (server_index, server) in servers.into_iter().enumerate() {
            if server.urls.is_empty() {
                return Err(IceConfigError::EmptyUrls { server_index });
            }

            for (url_index, input) in server.urls.iter().enumerate() {
                match parse_server_url(input, server_index, url_index)? {
                    ParsedIceUrl::Stun(uri) => {
                        if config.stun_server.is_none() {
                            config.stun_server = Some(uri);
                        }
                    },
                    ParsedIceUrl::Turn {
                        scheme,
                        host,
                        port,
                        transport,
                    } => {
                        let (Some(username), Some(password)) =
                            (server.username.as_deref(), server.credential.as_deref())
                        else {
                            return Err(IceConfigError::InvalidCredentials { server_index });
                        };

                        let uri = build_turn_uri(
                            scheme,
                            &host,
                            port,
                            transport,
                            username,
                            password,
                            server_index,
                            url_index,
                        )?;
                        config.turn_servers.push(uri);
                    },
                }
            }
        }

        Ok(config)
    }
}

#[derive(Clone, Copy)]
enum TurnTransport {
    Udp,
    Tcp,
}

enum ParsedIceUrl {
    Stun(String),
    Turn {
        scheme: &'static str,
        host: String,
        port: u16,
        transport: Option<TurnTransport>,
    },
}

fn parse_server_url(
    input: &str,
    server_index: usize,
    url_index: usize,
) -> Result<ParsedIceUrl, IceConfigError> {
    let (input_scheme, remainder) = input.split_once(':').ok_or(IceConfigError::InvalidUrl {
        server_index,
        url_index,
    })?;
    let scheme = input_scheme.to_ascii_lowercase();
    let supported = matches!(scheme.as_str(), "stun" | "turn" | "turns");
    if !supported {
        return Err(IceConfigError::UnsupportedScheme {
            server_index,
            url_index,
        });
    }

    // Browser RTCIceServer URLs commonly use the opaque `scheme:host` form,
    // while GStreamer and `url::Url` need an authority-based form.
    let authority_and_tail = remainder.strip_prefix("//").unwrap_or(remainder);
    let authority = authority_and_tail
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.contains('@') || authority.ends_with(':') {
        return Err(IceConfigError::InvalidUrl {
            server_index,
            url_index,
        });
    }
    let normalized = format!("{scheme}://{authority_and_tail}");
    let parsed = Url::parse(&normalized).map_err(|_| IceConfigError::InvalidUrl {
        server_index,
        url_index,
    })?;

    if parsed.host().is_none() || parsed.username() != "" || parsed.password().is_some() {
        return Err(IceConfigError::InvalidUrl {
            server_index,
            url_index,
        });
    }
    if !matches!(parsed.path(), "" | "/") || parsed.fragment().is_some() {
        return Err(IceConfigError::UnsupportedUrlComponent {
            server_index,
            url_index,
        });
    }

    let port = parsed.port();
    if port == Some(0) {
        return Err(IceConfigError::InvalidPort {
            server_index,
            url_index,
        });
    }
    let host = format_host(parsed.host().expect("host presence checked above"));

    match scheme.as_str() {
        "stun" => {
            if parsed.query().is_some() {
                return Err(IceConfigError::UnsupportedTransport {
                    server_index,
                    url_index,
                });
            }
            Ok(ParsedIceUrl::Stun(format!(
                "stun://{host}:{}",
                port.unwrap_or(3478)
            )))
        },
        "turn" | "turns" => {
            let transport = match parsed.query() {
                None => None,
                Some("transport=udp") => Some(TurnTransport::Udp),
                Some("transport=tcp") => Some(TurnTransport::Tcp),
                Some(_) => {
                    return Err(IceConfigError::UnsupportedTransport {
                        server_index,
                        url_index,
                    });
                },
            };

            // `webrtcbin` maps `turns` to TURN over TLS/TCP and ignores its
            // transport query when configuring libnice, so accepting `udp`
            // here would silently change the requested transport.
            if scheme == "turns" && matches!(transport, Some(TurnTransport::Udp)) {
                return Err(IceConfigError::UnsupportedTransport {
                    server_index,
                    url_index,
                });
            }

            Ok(ParsedIceUrl::Turn {
                scheme: if scheme == "turn" { "turn" } else { "turns" },
                host,
                port: port.unwrap_or(if scheme == "turn" { 3478 } else { 5349 }),
                transport,
            })
        },
        _ => unreachable!("unsupported ICE schemes return before parsing"),
    }
}

fn format_host(host: Host<&str>) -> String {
    match host {
        Host::Domain(domain) => domain.to_owned(),
        Host::Ipv4(address) => format_ipv4(address),
        Host::Ipv6(address) => format_ipv6(address),
    }
}

fn format_ipv4(address: Ipv4Addr) -> String {
    address.to_string()
}

fn format_ipv6(address: Ipv6Addr) -> String {
    format!("[{address}]")
}

fn build_turn_uri(
    scheme: &str,
    host: &str,
    port: u16,
    transport: Option<TurnTransport>,
    username: &str,
    password: &str,
    server_index: usize,
    url_index: usize,
) -> Result<String, IceConfigError> {
    // Let the URL crate apply its user-info percent-encoding rules. The
    // resulting encoded fields are reassembled explicitly to avoid adding the
    // trailing slash that URL serialization normally inserts.
    let mut uri = Url::parse(&format!("{scheme}://{host}:{port}")).map_err(|_| {
        IceConfigError::InvalidUrl {
            server_index,
            url_index,
        }
    })?;
    // `url::Url` preserves valid-looking percent escapes in user-info, but
    // RTCIceServer credentials are decoded strings. Escape literal percent
    // signs first so a value such as `%2F` is not interpreted by GStreamer as
    // a slash during URI decoding.
    let encoded_username = username.replace('%', "%25");
    let encoded_password = password.replace('%', "%25");
    uri.set_username(&encoded_username)
        .map_err(|_| IceConfigError::InvalidCredentials { server_index })?;
    uri.set_password(Some(&encoded_password))
        .map_err(|_| IceConfigError::InvalidCredentials { server_index })?;

    let query = match transport {
        None => "",
        Some(TurnTransport::Udp) => "?transport=udp",
        Some(TurnTransport::Tcp) => "?transport=tcp",
    };

    Ok(format!(
        "{scheme}://{}:{}@{host}:{port}{query}",
        uri.username(),
        uri.password().unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests {
    use super::{IceConfigError, IceConfiguration, IceServer, IceTransportPolicy};

    fn server(urls: &[&str], username: Option<&str>, credential: Option<&str>) -> IceServer {
        IceServer {
            urls: urls.iter().map(|url| (*url).to_owned()).collect(),
            username: username.map(str::to_owned),
            credential: credential.map(str::to_owned),
        }
    }

    #[test]
    fn stun_uri_gets_explicit_default_port() {
        let config = IceConfiguration::new(
            vec![server(&["stun:stun.example.org"], None, None)],
            IceTransportPolicy::All,
        )
        .unwrap();

        assert_eq!(
            config.stun_server.as_deref(),
            Some("stun://stun.example.org:3478")
        );
        assert!(config.turn_servers.is_empty());
    }

    #[test]
    fn stun_uri_preserves_explicit_port_and_ipv6() {
        let config = IceConfiguration::new(
            vec![server(&["stun:[2001:db8::1]:19302"], None, None)],
            IceTransportPolicy::All,
        )
        .unwrap();

        assert_eq!(
            config.stun_server.as_deref(),
            Some("stun://[2001:db8::1]:19302")
        );
    }

    #[test]
    fn turn_udp_uri_gets_default_port_and_escapes_credentials() {
        let config = IceConfiguration::new(
            vec![server(
                &["turn:turn.example.org"],
                Some("user%:name@site"),
                Some("p@ss:word%"),
            )],
            IceTransportPolicy::All,
        )
        .unwrap();

        assert_eq!(
            config.turn_servers,
            ["turn://user%25%3Aname%40site:p%40ss%3Aword%25@turn.example.org:3478"]
        );
    }

    #[test]
    fn turn_tcp_uri_preserves_transport_and_ipv6() {
        let config = IceConfiguration::new(
            vec![server(
                &["turn:[2001:db8::2]:5349?transport=tcp"],
                Some("u"),
                Some("p"),
            )],
            IceTransportPolicy::All,
        )
        .unwrap();

        assert_eq!(
            config.turn_servers,
            ["turn://u:p@[2001:db8::2]:5349?transport=tcp"]
        );
    }

    #[test]
    fn turns_uri_gets_tls_default_port_and_policy_is_preserved() {
        let config = IceConfiguration::new(
            vec![server(&["turns:turn.example.org"], Some("u"), Some("p"))],
            IceTransportPolicy::Relay,
        )
        .unwrap();

        assert_eq!(config.turn_servers, ["turns://u:p@turn.example.org:5349"]);
        assert_eq!(config.policy, IceTransportPolicy::Relay);
    }

    #[test]
    fn multiple_turn_urls_are_all_returned() {
        let config = IceConfiguration::new(
            vec![server(
                &["turn:turn-a.example.org", "turns:turn-b.example.org"],
                Some("u"),
                Some("p"),
            )],
            IceTransportPolicy::All,
        )
        .unwrap();

        assert_eq!(
            config.turn_servers,
            [
                "turn://u:p@turn-a.example.org:3478",
                "turns://u:p@turn-b.example.org:5349"
            ]
        );
    }

    #[test]
    fn first_stun_url_is_used_when_multiple_are_configured() {
        let config = IceConfiguration::new(
            vec![server(
                &[
                    "stun:first.example.org",
                    "stun:second.example.org",
                    "turn:relay.example.org",
                ],
                Some("user"),
                Some("password"),
            )],
            IceTransportPolicy::All,
        )
        .unwrap();

        assert_eq!(
            config.stun_server.as_deref(),
            Some("stun://first.example.org:3478")
        );
        assert_eq!(
            config.turn_servers,
            ["turn://user:password@relay.example.org:3478"]
        );
    }

    #[test]
    fn urls_after_the_first_stun_are_still_validated() {
        let error = IceConfiguration::new(
            vec![server(
                &["stun:first.example.org", "https://unsupported.example.org"],
                None,
                None,
            )],
            IceTransportPolicy::All,
        )
        .unwrap_err();

        assert_eq!(
            error,
            IceConfigError::UnsupportedScheme {
                server_index: 0,
                url_index: 1,
            }
        );
    }

    #[test]
    fn unsupported_schemes_and_malformed_urls_are_reported() {
        let unsupported = IceConfiguration::new(
            vec![server(&["https://example.org"], None, None)],
            IceTransportPolicy::All,
        )
        .unwrap_err();
        let malformed = IceConfiguration::new(
            vec![server(&["stun:not a host:bad port"], None, None)],
            IceTransportPolicy::All,
        )
        .unwrap_err();
        let empty_port = IceConfiguration::new(
            vec![server(&["stun:stun.example.org:"], None, None)],
            IceTransportPolicy::All,
        )
        .unwrap_err();
        let secure_stun = IceConfiguration::new(
            vec![server(&["stuns:stun.example.org"], None, None)],
            IceTransportPolicy::All,
        )
        .unwrap_err();

        assert!(matches!(
            unsupported,
            IceConfigError::UnsupportedScheme { .. }
        ));
        assert!(matches!(malformed, IceConfigError::InvalidUrl { .. }));
        assert!(matches!(empty_port, IceConfigError::InvalidUrl { .. }));
        assert!(matches!(
            secure_stun,
            IceConfigError::UnsupportedScheme { .. }
        ));
    }

    #[test]
    fn missing_turn_credentials_are_rejected_without_echoing_input() {
        let error = IceConfiguration::new(
            vec![server(
                &["turn:turn.example.org"],
                Some("secret-user"),
                None,
            )],
            IceTransportPolicy::All,
        )
        .unwrap_err();

        assert_eq!(
            error,
            IceConfigError::InvalidCredentials { server_index: 0 }
        );
        assert!(!error.to_string().contains("secret-user"));
    }

    #[test]
    fn configuration_debug_output_redacts_turn_credentials() {
        let config = IceConfiguration::new(
            vec![server(
                &["turn:turn.example.org"],
                Some("private-user"),
                Some("private-password"),
            )],
            IceTransportPolicy::All,
        )
        .unwrap();
        let debug = format!("{config:?}");

        assert!(debug.contains("[REDACTED; 1 configured]"));
        assert!(!debug.contains("private-user"));
        assert!(!debug.contains("private-password"));
    }

    #[test]
    fn turns_udp_and_unknown_queries_are_rejected() {
        let secure_udp = IceConfiguration::new(
            vec![server(
                &["turns:turn.example.org?transport=udp"],
                Some("u"),
                Some("p"),
            )],
            IceTransportPolicy::All,
        )
        .unwrap_err();
        let unknown_query = IceConfiguration::new(
            vec![server(
                &["turn:turn.example.org?transport=sctp"],
                Some("u"),
                Some("p"),
            )],
            IceTransportPolicy::All,
        )
        .unwrap_err();

        assert!(matches!(
            secure_udp,
            IceConfigError::UnsupportedTransport { .. }
        ));
        assert!(matches!(
            unknown_query,
            IceConfigError::UnsupportedTransport { .. }
        ));
    }
}
