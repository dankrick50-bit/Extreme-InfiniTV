// Finds Chromecasts and DLNA smart TVs on the home network.
//
// SSDP works on every platform, Android included: the search goes out as multicast but each TV
// answers straight back to our socket. Desktop additionally browses mDNS for Chromecasts.

use std::net::Ipv4Addr;
use std::time::Duration;

use super::chromecast::{self, CAST_PORT};
use super::dlna;
use super::session::{DlnaEndpoints, Target, TargetKind};
use super::xml::{tag_blocks, tag_text};

const SSDP_ADDRESS: (Ipv4Addr, u16) = (Ipv4Addr::new(239, 255, 255, 250), 1900);
const ST_MEDIA_RENDERER: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
/// Chromecasts answer this one; so do many other TVs, which the Cast port check then rules out.
const ST_DIAL: &str = "urn:dial-multiscreen-org:service:dial:1";

pub const DISCOVERY_WINDOW: Duration = Duration::from_millis(3000);
const DESCRIPTION_TIMEOUT: Duration = Duration::from_millis(2500);
const CAST_CHECK_TIMEOUT: Duration = Duration::from_millis(1500);
const MAX_DESCRIPTION_BYTES: usize = 256 * 1024;
const MAX_NAME_LEN: usize = 64;
const MAX_SSDP_HITS: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub struct FoundDevice {
    pub kind: TargetKind,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub uid: Option<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub dlna: Option<DlnaEndpoints>,
}

impl FoundDevice {
    /// Handle the picker passes back to say which scan result it means.
    pub fn found_id(&self) -> String {
        let kind = match self.kind {
            TargetKind::Chromecast => "chromecast",
            TargetKind::Dlna => "dlna",
        };
        format!("{kind}:{}:{}", self.host, self.port)
    }
}

fn bounded_name(raw: &str, fallback: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.chars().take(MAX_NAME_LEN).collect()
    }
}

/// Cast ids and UPnP UDNs name the same device in different spellings.
pub fn normalize_uid(raw: &str) -> Option<String> {
    let lowered = raw.trim().to_ascii_lowercase();
    let stripped: String = lowered.strip_prefix("uuid:").unwrap_or(&lowered).chars().filter(|character| *character != '-').collect();
    (!stripped.is_empty()).then_some(stripped)
}

/// Whether a scan result is the TV we saved earlier: by identity when both sides have one, else by name.
pub fn same_device(target: &Target, found: &FoundDevice) -> bool {
    if target.kind != found.kind {
        return false;
    }
    match (&target.uid, &found.uid) {
        (Some(saved), Some(seen)) => saved == seen,
        _ => target.name == found.name,
    }
}

// ---------------------------------------------------------------------------
// SSDP
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct SsdpHit {
    location: String,
    search_target: String,
    source: Ipv4Addr,
}

fn search_message(search_target: &str) -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {search_target}\r\n\r\n"
    )
}

/// Returns (location, search target) from a search reply; anything else is ignored.
fn parse_ssdp_response(text: &str) -> Option<(String, String)> {
    let mut lines = text.lines();
    let status = lines.next()?;
    if !status.starts_with("HTTP/1.") || !status.contains(" 200") {
        return None;
    }
    let mut location = None;
    let mut search_target = String::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "location" => location = Some(value.trim().to_string()),
            "st" => search_target = value.trim().to_string(),
            _ => {}
        }
    }
    Some((location?, search_target))
}

/// A TV must describe itself from its own address: a reply pointing elsewhere is not followed.
fn url_on_host(raw: &str, expected: Ipv4Addr) -> Option<tauri::Url> {
    let url = tauri::Url::parse(raw).ok()?;
    if url.scheme() != "http" {
        return None;
    }
    let host: Ipv4Addr = url.host_str()?.parse().ok()?;
    (host == expected).then_some(url)
}

#[cfg(not(target_os = "ios"))]
fn lan_addresses() -> Vec<Ipv4Addr> {
    let mut addresses: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .iter()
        .filter_map(|interface| match &interface.addr {
            if_addrs::IfAddr::V4(v4) if v4.ip.is_private() => Some(v4.ip),
            _ => None,
        })
        .collect();
    addresses.sort();
    addresses.dedup();
    if addresses.is_empty() {
        addresses.push(Ipv4Addr::UNSPECIFIED);
    }
    addresses
}

#[cfg(target_os = "ios")]
fn lan_addresses() -> Vec<Ipv4Addr> {
    vec![Ipv4Addr::UNSPECIFIED]
}

async fn search_from(local: Ipv4Addr, window: Duration) -> Vec<SsdpHit> {
    let Ok(socket) = tokio::net::UdpSocket::bind((local, 0)).await else {
        return Vec::new();
    };
    let _ = socket.set_multicast_ttl_v4(4);
    // Sent twice: a lone UDP search is easily lost on Wi-Fi.
    for round in 0..2 {
        for search_target in [ST_MEDIA_RENDERER, ST_DIAL] {
            let _ = socket.send_to(search_message(search_target).as_bytes(), SSDP_ADDRESS).await;
        }
        if round == 0 {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    let deadline = tokio::time::Instant::now() + window;
    let mut hits = Vec::new();
    let mut buffer = [0u8; 2048];
    while hits.len() < MAX_SSDP_HITS {
        let Ok(Ok((length, from))) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await else {
            break;
        };
        let std::net::IpAddr::V4(source) = from.ip() else {
            continue;
        };
        // Only the home network is searched; a reply from anywhere else is not a TV in this house.
        if !source.is_private() {
            continue;
        }
        let Some((location, search_target)) = parse_ssdp_response(&String::from_utf8_lossy(&buffer[..length])) else {
            continue;
        };
        hits.push(SsdpHit { location, search_target, source });
    }
    hits
}

async fn ssdp_search(window: Duration) -> Vec<SsdpHit> {
    let searches = lan_addresses().into_iter().map(|local| search_from(local, window));
    let mut hits: Vec<SsdpHit> = futures_util::future::join_all(searches).await.into_iter().flatten().collect();
    // One description fetch per TV and role, however many times it answered.
    hits.sort_by(|left, right| (&left.location, &left.search_target).cmp(&(&right.location, &right.search_target)));
    hits.dedup_by(|left, right| left.location == right.location && left.search_target == right.search_target);
    hits
}

// ---------------------------------------------------------------------------
// Device descriptions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default)]
struct Description {
    friendly_name: Option<String>,
    manufacturer: Option<String>,
    model: Option<String>,
    udn: Option<String>,
    av_transport: Option<(String, String)>,
    rendering_control: Option<(String, String)>,
    connection_manager: Option<(String, String)>,
}

/// Reads the fields casting needs. Control addresses that leave the TV's own host are dropped.
fn parse_description(xml: &str, location: &tauri::Url) -> Description {
    let host = location.host_str().and_then(|host| host.parse::<Ipv4Addr>().ok());
    let base = tag_text(xml, "URLBase")
        .and_then(|base| host.and_then(|host| url_on_host(&base, host)))
        .unwrap_or_else(|| location.clone());
    let control_url = |raw: &str| -> Option<String> {
        let joined = base.join(raw.trim()).ok()?;
        url_on_host(joined.as_str(), host?).map(String::from)
    };

    let mut description = Description {
        friendly_name: tag_text(xml, "friendlyName"),
        manufacturer: tag_text(xml, "manufacturer"),
        model: tag_text(xml, "modelName"),
        udn: tag_text(xml, "UDN").and_then(|udn| normalize_uid(&udn)),
        ..Description::default()
    };
    for service in tag_blocks(xml, "service") {
        let (Some(service_type), Some(control)) = (tag_text(service, "serviceType"), tag_text(service, "controlURL")) else {
            continue;
        };
        let Some(control) = control_url(&control) else {
            continue;
        };
        if service_type.contains(":service:AVTransport:") && description.av_transport.is_none() {
            description.av_transport = Some((control, service_type));
        } else if service_type.contains(":service:RenderingControl:") && description.rendering_control.is_none() {
            description.rendering_control = Some((control, service_type));
        } else if service_type.contains(":service:ConnectionManager:") && description.connection_manager.is_none() {
            description.connection_manager = Some((control, service_type));
        }
    }
    description
}

fn http_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(DESCRIPTION_TIMEOUT)
        .connect_timeout(Duration::from_millis(1500))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .ok()
}

async fn fetch_description(client: &reqwest::Client, location: &tauri::Url) -> Option<Description> {
    let mut response = client.get(location.as_str()).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_DESCRIPTION_BYTES {
            return None;
        }
    }
    Some(parse_description(&String::from_utf8_lossy(&body), location))
}

fn dlna_device(description: &Description, location: &tauri::Url) -> Option<FoundDevice> {
    let (av_transport_url, av_transport_type) = description.av_transport.clone()?;
    let port = tauri::Url::parse(&av_transport_url).ok()?.port_or_known_default()?;
    let (rendering_control_url, rendering_control_type) = match description.rendering_control.clone() {
        Some((url, service_type)) => (Some(url), Some(service_type)),
        None => (None, None),
    };
    Some(FoundDevice {
        kind: TargetKind::Dlna,
        name: bounded_name(description.friendly_name.as_deref().unwrap_or_default(), "Smart TV"),
        host: location.host_str()?.to_string(),
        port,
        uid: description.udn.clone(),
        manufacturer: description.manufacturer.clone(),
        model: description.model.clone(),
        dlna: Some(DlnaEndpoints {
            av_transport_url,
            av_transport_type,
            rendering_control_url,
            rendering_control_type,
            accepted_mimes: Vec::new(),
        }),
    })
}

fn chromecast_device(description: &Description, host: Ipv4Addr) -> FoundDevice {
    FoundDevice {
        kind: TargetKind::Chromecast,
        name: bounded_name(description.friendly_name.as_deref().unwrap_or_default(), "Chromecast"),
        host: host.to_string(),
        port: CAST_PORT,
        uid: description.udn.clone(),
        manufacturer: description.manufacturer.clone(),
        model: description.model.clone(),
        dlna: None,
    }
}

async fn resolve_hit(client: &reqwest::Client, hit: SsdpHit) -> Option<FoundDevice> {
    let location = url_on_host(&hit.location, hit.source)?;
    let description = fetch_description(client, &location).await?;
    if hit.search_target.contains("MediaRenderer") {
        let mut device = dlna_device(&description, &location)?;
        if let Some((control_url, service_type)) = &description.connection_manager {
            if let Some(mimes) = dlna::accepted_mimes(client, control_url, service_type).await {
                if dlna::is_audio_only(&mimes) {
                    log::info!("[cast-bridge] skipping {}: it plays audio only", device.name);
                    return None;
                }
                if let Some(endpoints) = device.dlna.as_mut() {
                    endpoints.accepted_mimes = dlna::video_mimes(mimes);
                }
            }
        }
        return Some(device);
    }
    if !chromecast::can_connect(&hit.source.to_string(), CAST_PORT, CAST_CHECK_TIMEOUT).await {
        return None;
    }
    Some(chromecast_device(&description, hit.source))
}

// ---------------------------------------------------------------------------
// mDNS (desktop only; Android has no multicast receive without a platform lock)
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "android", target_os = "ios")))]
async fn mdns_chromecasts(window: Duration) -> Vec<FoundDevice> {
    const SERVICE: &str = "_googlecast._tcp.local.";
    /// Capability bit for "has a screen"; speakers and audio groups leave it clear.
    const VIDEO_OUT: u32 = 1;

    let Ok(daemon) = mdns_sd::ServiceDaemon::new() else {
        return Vec::new();
    };
    let Ok(events) = daemon.browse(SERVICE) else {
        let _ = daemon.shutdown();
        return Vec::new();
    };
    let deadline = tokio::time::Instant::now() + window;
    let mut found: Vec<FoundDevice> = Vec::new();
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, events.recv_async()).await {
        let mdns_sd::ServiceEvent::ServiceResolved(resolved) = event else {
            continue;
        };
        let capabilities = resolved.get_property_val_str("ca").and_then(|value| value.parse::<u32>().ok());
        if capabilities.is_some_and(|capabilities| capabilities & VIDEO_OUT == 0) {
            continue;
        }
        let address = resolved
            .get_addresses()
            .iter()
            .map(mdns_sd::ScopedIp::to_ip_addr)
            .find_map(|address| match address {
                std::net::IpAddr::V4(v4) if v4.is_private() => Some(v4),
                _ => None,
            });
        let Some(address) = address else {
            continue;
        };
        let device = FoundDevice {
            kind: TargetKind::Chromecast,
            name: bounded_name(resolved.get_property_val_str("fn").unwrap_or_default(), "Chromecast"),
            host: address.to_string(),
            port: resolved.get_port(),
            uid: resolved.get_property_val_str("id").and_then(normalize_uid),
            manufacturer: None,
            model: resolved.get_property_val_str("md").map(str::to_string),
            dlna: None,
        };
        if !found.iter().any(|existing| existing.found_id() == device.found_id()) {
            found.push(device);
        }
    }
    let _ = daemon.stop_browse(SERVICE);
    let _ = daemon.shutdown();
    found
}

#[cfg(any(target_os = "android", target_os = "ios"))]
async fn mdns_chromecasts(_window: Duration) -> Vec<FoundDevice> {
    Vec::new()
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Chromecasts from mDNS first (they carry the Cast identity), then whatever SSDP added.
fn merge_found(mdns: Vec<FoundDevice>, ssdp: Vec<FoundDevice>) -> Vec<FoundDevice> {
    let mut merged = mdns;
    for device in ssdp {
        if !merged.iter().any(|existing| existing.found_id() == device.found_id()) {
            merged.push(device);
        }
    }
    merged.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()).then(left.host.cmp(&right.host)));
    merged
}

pub async fn discover(window: Duration) -> Vec<FoundDevice> {
    let Some(client) = http_client() else {
        return Vec::new();
    };
    let ssdp = async {
        let hits = ssdp_search(window).await;
        let resolved = futures_util::future::join_all(hits.into_iter().map(|hit| resolve_hit(&client, hit))).await;
        resolved.into_iter().flatten().collect::<Vec<FoundDevice>>()
    };
    let (mdns, mut ssdp, swept) = tokio::join!(mdns_chromecasts(window), ssdp, sweep_chromecasts(&client));
    // Sweep results last: the same Chromecast found another way carries a better name or identity.
    ssdp.extend(swept);
    let found = merge_found(mdns, ssdp);
    log::info!("[cast-bridge] discovery found {} tv(s)", found.len());
    found
}

// ---------------------------------------------------------------------------
// Unicast sweep for Chromecasts (firewalls and some routers swallow the multicast searches above;
// this PC's own firewall does for mDNS, and Android has no mDNS here at all)
// ---------------------------------------------------------------------------

const SWEEP_CONNECT_TIMEOUT: Duration = Duration::from_millis(400);
const SWEEP_CONCURRENCY: usize = 128;
const SWEEP_HOST_CAP: usize = 512;

/// Every other address on each local /24, the size home networks are; wider subnets are clamped to it.
fn sweep_candidates(own_addresses: &[Ipv4Addr]) -> Vec<Ipv4Addr> {
    let mut candidates = Vec::new();
    for own in own_addresses.iter().filter(|own| own.is_private()) {
        let network = u32::from(*own) & 0xffff_ff00;
        for last in 1..=254u32 {
            let host = Ipv4Addr::from(network | last);
            if own_addresses.contains(&host) || candidates.contains(&host) {
                continue;
            }
            if candidates.len() >= SWEEP_HOST_CAP {
                return candidates;
            }
            candidates.push(host);
        }
    }
    candidates
}

/// The name lives on the DIAL port, which some models keep closed; "Chromecast" then has to do.
async fn name_chromecast(client: &reqwest::Client, host: Ipv4Addr) -> FoundDevice {
    let description = match tauri::Url::parse(&format!("http://{host}:8008/ssdp/device-desc.xml")) {
        Ok(location) => fetch_description(client, &location).await.unwrap_or_default(),
        Err(_) => Description::default(),
    };
    chromecast_device(&description, host)
}

/// Knocks on the Cast port of every local address, then keeps the ones that really speak Cast.
async fn sweep_chromecasts(client: &reqwest::Client) -> Vec<FoundDevice> {
    let candidates = sweep_candidates(&lan_addresses());
    let limit = std::sync::Arc::new(tokio::sync::Semaphore::new(SWEEP_CONCURRENCY));
    let knocks = candidates.into_iter().map(|host| {
        let limit = limit.clone();
        async move {
            let _permit = limit.acquire().await.ok()?;
            let connected = tokio::time::timeout(SWEEP_CONNECT_TIMEOUT, tokio::net::TcpStream::connect((host, CAST_PORT))).await;
            matches!(connected, Ok(Ok(_))).then_some(host)
        }
    });
    let open: Vec<Ipv4Addr> = futures_util::future::join_all(knocks).await.into_iter().flatten().collect();
    // Other products use this port too (Fire TV does, without encryption); the Cast handshake tells them apart.
    let confirmed = open.into_iter().map(|host| async move {
        if !chromecast::can_connect(&host.to_string(), CAST_PORT, CAST_CHECK_TIMEOUT).await {
            return None;
        }
        Some(name_chromecast(client, host).await)
    });
    futures_util::future::join_all(confirmed).await.into_iter().flatten().collect()
}

/// For adding a Chromecast by typing its address when the network hides it from discovery.
pub async fn describe_chromecast_at(host: Ipv4Addr) -> Option<FoundDevice> {
    if !chromecast::can_connect(&host.to_string(), CAST_PORT, Duration::from_secs(4)).await {
        return None;
    }
    match http_client() {
        Some(client) => Some(name_chromecast(&client, host).await),
        None => Some(chromecast_device(&Description::default(), host)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LG_DESCRIPTION: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0" xmlns:dlna="urn:schemas-dlna-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>[LG] webOS TV OLED55</friendlyName>
    <manufacturer>LG Electronics</manufacturer>
    <modelName>LG TV</modelName>
    <UDN>uuid:1F2E3D4C-aaaa-bbbb-cccc-0123456789ab</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <SCPDURL>/AVTransport/scpd.xml</SCPDURL>
        <controlURL>/AVTransport/control</controlURL>
        <eventSubURL>/AVTransport/event</eventSubURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <controlURL>/ConnectionManager/control</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <controlURL>RenderingControl/control</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    fn location(raw: &str) -> tauri::Url {
        tauri::Url::parse(raw).unwrap()
    }

    #[test]
    fn ssdp_response_yields_location_and_search_target() {
        let reply = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nLocation: http://192.168.1.20:1424/desc.xml\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\nUSN: uuid:abc::urn:x\r\n\r\n";
        let (location, search_target) = parse_ssdp_response(reply).unwrap();
        assert_eq!(location, "http://192.168.1.20:1424/desc.xml");
        assert_eq!(search_target, ST_MEDIA_RENDERER);
    }

    #[test]
    fn ssdp_ignores_announcements_and_replies_without_a_location() {
        assert!(parse_ssdp_response("NOTIFY * HTTP/1.1\r\nLOCATION: http://192.168.1.20/d.xml\r\n\r\n").is_none());
        assert!(parse_ssdp_response("HTTP/1.1 200 OK\r\nST: x\r\n\r\n").is_none());
        assert!(parse_ssdp_response("").is_none());
    }

    #[test]
    fn a_location_must_be_plain_http_on_the_replying_address() {
        let tv = Ipv4Addr::new(192, 168, 1, 20);
        assert!(url_on_host("http://192.168.1.20:1424/desc.xml", tv).is_some());
        assert!(url_on_host("http://192.168.1.99:1424/desc.xml", tv).is_none());
        assert!(url_on_host("https://192.168.1.20/desc.xml", tv).is_none());
        assert!(url_on_host("http://tv.example.com/desc.xml", tv).is_none());
        assert!(url_on_host("file:///etc/passwd", tv).is_none());
    }

    #[test]
    fn description_resolves_relative_control_addresses() {
        let location = location("http://192.168.1.20:1424/desc.xml");
        let description = parse_description(LG_DESCRIPTION, &location);
        assert_eq!(description.friendly_name.as_deref(), Some("[LG] webOS TV OLED55"));
        assert_eq!(description.manufacturer.as_deref(), Some("LG Electronics"));
        assert_eq!(description.udn.as_deref(), Some("1f2e3d4caaaabbbbcccc0123456789ab"));
        assert_eq!(
            description.av_transport,
            Some(("http://192.168.1.20:1424/AVTransport/control".to_string(), "urn:schemas-upnp-org:service:AVTransport:1".to_string()))
        );
        assert_eq!(
            description.rendering_control.map(|(url, _)| url).as_deref(),
            Some("http://192.168.1.20:1424/RenderingControl/control")
        );
    }

    #[test]
    fn description_honours_a_url_base_on_the_same_host() {
        let xml = r#"<root><URLBase>http://192.168.1.30:9197/dmr/</URLBase><device><friendlyName>Samsung Q60</friendlyName>
            <serviceList><service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
            <controlURL>upnp/control/AVTransport1</controlURL></service></serviceList></device></root>"#;
        let description = parse_description(xml, &location("http://192.168.1.30:9197/dmr"));
        assert_eq!(description.av_transport.map(|(url, _)| url).as_deref(), Some("http://192.168.1.30:9197/dmr/upnp/control/AVTransport1"));
    }

    #[test]
    fn control_addresses_on_another_host_are_dropped() {
        let xml = r#"<root><URLBase>http://10.9.9.9:80/</URLBase><device><friendlyName>Odd</friendlyName><serviceList>
            <service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType><controlURL>http://203.0.113.5/ctl</controlURL></service>
            </serviceList></device></root>"#;
        let location = location("http://192.168.1.40:8080/d.xml");
        let description = parse_description(xml, &location);
        assert_eq!(description.av_transport, None);
        assert!(dlna_device(&description, &location).is_none());
    }

    #[test]
    fn a_renderer_description_becomes_a_dlna_device() {
        let location = location("http://192.168.1.20:1424/desc.xml");
        let device = dlna_device(&parse_description(LG_DESCRIPTION, &location), &location).unwrap();
        assert_eq!(device.kind, TargetKind::Dlna);
        assert_eq!(device.host, "192.168.1.20");
        assert_eq!(device.port, 1424);
        assert_eq!(device.found_id(), "dlna:192.168.1.20:1424");
        assert!(device.dlna.unwrap().rendering_control_url.is_some());
    }

    #[test]
    fn the_sweep_covers_the_local_subnet_but_not_this_device() {
        let own = [Ipv4Addr::new(192, 168, 1, 86)];
        let candidates = sweep_candidates(&own);
        assert_eq!(candidates.len(), 253);
        assert_eq!(candidates.first(), Some(&Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(candidates.last(), Some(&Ipv4Addr::new(192, 168, 1, 254)));
        assert!(!candidates.contains(&own[0]));
    }

    #[test]
    fn the_sweep_never_leaves_the_home_network() {
        assert!(sweep_candidates(&[Ipv4Addr::UNSPECIFIED]).is_empty());
        assert!(sweep_candidates(&[Ipv4Addr::new(8, 8, 8, 8)]).is_empty());
        // Two adapters on the same subnet are swept once; a third subnet stops at the cap.
        let same_subnet = sweep_candidates(&[Ipv4Addr::new(10, 0, 0, 5), Ipv4Addr::new(10, 0, 0, 6)]);
        assert_eq!(same_subnet.len(), 252);
        let three = sweep_candidates(&[Ipv4Addr::new(10, 0, 0, 5), Ipv4Addr::new(10, 0, 1, 5), Ipv4Addr::new(10, 0, 2, 5)]);
        assert_eq!(three.len(), SWEEP_HOST_CAP);
    }

    #[test]
    fn uid_spellings_normalize_to_the_same_value() {
        assert_eq!(normalize_uid("uuid:1F2E3D4C-AAAA-bbbb-cccc-0123456789ab"), normalize_uid("1f2e3d4caaaabbbbcccc0123456789ab"));
        assert_eq!(normalize_uid("  "), None);
    }

    fn found(kind: TargetKind, name: &str, host: &str, uid: Option<&str>) -> FoundDevice {
        FoundDevice {
            kind,
            name: name.to_string(),
            host: host.to_string(),
            port: CAST_PORT,
            uid: uid.map(str::to_string),
            manufacturer: None,
            model: None,
            dlna: None,
        }
    }

    #[test]
    fn merge_keeps_the_mdns_entry_for_a_chromecast_seen_twice() {
        let mdns = vec![found(TargetKind::Chromecast, "Living Room TV", "192.168.1.50", Some("abc"))];
        let ssdp = vec![
            found(TargetKind::Chromecast, "Living Room TV", "192.168.1.50", None),
            found(TargetKind::Dlna, "Bedroom TV", "192.168.1.60", Some("def")),
        ];
        let merged = merge_found(mdns, ssdp);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "Bedroom TV");
        assert_eq!(merged[1].uid.as_deref(), Some("abc"));
    }

    #[test]
    fn same_device_prefers_identity_over_name() {
        let target = Target {
            id: "ext-1".to_string(),
            kind: TargetKind::Dlna,
            name: "Bedroom TV".to_string(),
            host: "192.168.1.60".to_string(),
            port: 1424,
            key: "k".repeat(32),
            uid: Some("def".to_string()),
            manufacturer: None,
            dlna: None,
        };
        assert!(same_device(&target, &found(TargetKind::Dlna, "Renamed TV", "192.168.1.77", Some("def"))));
        assert!(!same_device(&target, &found(TargetKind::Dlna, "Bedroom TV", "192.168.1.60", Some("other"))));
        assert!(same_device(&target, &found(TargetKind::Dlna, "Bedroom TV", "192.168.1.60", None)));
        assert!(!same_device(&target, &found(TargetKind::Chromecast, "Bedroom TV", "192.168.1.60", Some("def"))));
    }

    /// Looks at the real network; run by hand with `cargo test live_discovery -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_discovery_lists_the_tvs_on_this_network() {
        let found = discover(DISCOVERY_WINDOW).await;
        println!("found {} device(s)", found.len());
        for device in &found {
            println!(
                "  {:?} name={:?} host={} port={} model={:?} manufacturer={:?} uid={} formats={:?}",
                device.kind,
                device.name,
                device.host,
                device.port,
                device.model,
                device.manufacturer,
                device.uid.is_some(),
                device.dlna.as_ref().map(|endpoints| endpoints.accepted_mimes.join(" "))
            );
        }
    }
}
