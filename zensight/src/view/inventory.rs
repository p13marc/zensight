//! First-class passive **inventory** view (#120).
//!
//! Promotes the netring passive asset inventory — previously buried in a
//! per-host card on the netring sensor device — to a top-level section, and adds
//! a unified **fingerprint explorer** that merges the TLS (JA4/JA3/SNI), QUIC
//! (SNI) and SSH (HASSH) inventories into one group-by-fingerprint table. Both
//! are the analyst's discovery surface for hosts/infrastructure that emit no
//! telemetry of their own.
//!
//! Data is fetched on view-open from the netring `@/query/{assets,tls,quic,ssh}`
//! queryables (global, not per-source) and held at app level; this module owns
//! the state shape, the pure sort/merge logic, and the rendering.

use iced::widget::{Column, button, column, container, pick_list, row, scrollable, text};
use iced::{Element, Length, Theme};

use zensight_common::{AssetRecord, Ja4hRecord, QuicRecord, SshRecord, TlsRecord};

use crate::message::Message;
use crate::view::components::{card, empty_state, section_header};
use crate::view::formatting::format_timestamp;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// The four inventory tables fetched together on view-open. Carried in one
/// message so a single combined task populates the whole view.
#[derive(Debug, Clone, Default)]
pub struct InventoryData {
    pub assets: Vec<AssetRecord>,
    pub tls: Vec<TlsRecord>,
    pub quic: Vec<QuicRecord>,
    pub ssh: Vec<SshRecord>,
    /// JA4H HTTP-request fingerprints (#124). Empty unless the netring sensor was
    /// built with `--features ja4plus` and `collect.http_fp` is set.
    pub ja4h: Vec<Ja4hRecord>,
    /// Whether a netring sensor actually answered the `@/query/assets` queryable.
    /// `false` means asset collection is disabled on the sensor (`collect.assets`
    /// is opt-in); distinct from answering with an empty list (nothing seen yet).
    pub assets_responded: bool,
}

/// Sort order for the asset table (#120).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AssetSort {
    /// Most recently seen first (default).
    #[default]
    LastSeen,
    /// Alphabetical by vendor (blanks last).
    Vendor,
    /// Alphabetical by hostname (blanks last).
    Hostname,
}

impl AssetSort {
    pub const ALL: [AssetSort; 3] = [AssetSort::LastSeen, AssetSort::Vendor, AssetSort::Hostname];
    fn label(self) -> &'static str {
        match self {
            AssetSort::LastSeen => "last seen",
            AssetSort::Vendor => "vendor",
            AssetSort::Hostname => "hostname",
        }
    }
}

impl std::fmt::Display for AssetSort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// The kind of fingerprint in the unified explorer — drives the filter chips and
/// whether a row carries an allowlist-able host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FpKind {
    Ja4,
    Ja3,
    Ja4h,
    QuicSni,
    Hassh,
}

impl FpKind {
    pub const ALL: [FpKind; 5] = [
        FpKind::Ja4,
        FpKind::Ja3,
        FpKind::Ja4h,
        FpKind::QuicSni,
        FpKind::Hassh,
    ];
    pub fn label(self) -> &'static str {
        match self {
            FpKind::Ja4 => "JA4",
            FpKind::Ja3 => "JA3",
            FpKind::Ja4h => "JA4H",
            FpKind::QuicSni => "QUIC-SNI",
            FpKind::Hassh => "HASSH",
        }
    }
}

/// One row in the unified fingerprint explorer.
#[derive(Debug, Clone, PartialEq)]
pub struct Fingerprint {
    pub kind: FpKind,
    /// The fingerprint value (hash or SNI hostname).
    pub value: String,
    /// Secondary context (SNI for JA4/JA3, ALPN/version for QUIC, role/banner for SSH).
    pub detail: String,
    pub count: u64,
    /// Host/SLD this fingerprint can be allow-listed under (the SNI), if any.
    /// Only SNI-bearing rows are allow-listable since the netring allowlist is
    /// host-based.
    pub allowlist_host: Option<String>,
}

/// App-level inventory state (#120).
#[derive(Debug, Clone, Default)]
pub struct InventoryState {
    pub loading: bool,
    pub error: Option<String>,
    pub assets: Vec<AssetRecord>,
    /// Whether the netring sensor answered the assets query (see `InventoryData`).
    pub assets_responded: bool,
    pub fingerprints: Vec<Fingerprint>,
    pub asset_sort: AssetSort,
    /// Active fingerprint-kind filter (`None` = all kinds).
    pub fp_filter: Option<FpKind>,
}

impl InventoryState {
    /// Mark a fetch as in flight.
    pub fn loading(&mut self) {
        self.loading = true;
        self.error = None;
    }

    /// Store a combined fetch outcome.
    pub fn apply(&mut self, result: Result<InventoryData, String>) {
        self.loading = false;
        match result {
            Ok(data) => {
                self.fingerprints =
                    merge_fingerprints(&data.tls, &data.quic, &data.ssh, &data.ja4h);
                self.assets = data.assets;
                self.assets_responded = data.assets_responded;
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Assets in the active sort order (a sorted copy of references).
    pub fn sorted_assets(&self) -> Vec<&AssetRecord> {
        sort_assets(&self.assets, self.asset_sort)
    }

    /// Fingerprints filtered to the active kind (or all when `None`).
    pub fn filtered_fingerprints(&self) -> Vec<&Fingerprint> {
        self.fingerprints
            .iter()
            .filter(|f| self.fp_filter.is_none_or(|k| f.kind == k))
            .collect()
    }
}

/// Sort asset references by the chosen order. `LastSeen` is descending (newest
/// first); the text sorts are ascending with blanks pushed last. Pure + testable.
pub fn sort_assets(assets: &[AssetRecord], sort: AssetSort) -> Vec<&AssetRecord> {
    let mut out: Vec<&AssetRecord> = assets.iter().collect();
    match sort {
        AssetSort::LastSeen => out.sort_by_key(|a| std::cmp::Reverse(a.last_seen)),
        AssetSort::Vendor => {
            out.sort_by(|a, b| blanks_last(a.vendor.as_deref(), b.vendor.as_deref()))
        }
        AssetSort::Hostname => {
            out.sort_by(|a, b| blanks_last(a.hostname.as_deref(), b.hostname.as_deref()))
        }
    }
    out
}

/// Compare two optional strings case-insensitively, ordering `None`/empty last.
fn blanks_last(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    let key = |s: Option<&str>| -> (bool, String) {
        match s.map(str::trim).filter(|s| !s.is_empty()) {
            Some(v) => (false, v.to_lowercase()),
            None => (true, String::new()),
        }
    };
    key(a).cmp(&key(b))
}

/// Merge the per-protocol fingerprint inventories into one group-by-fingerprint
/// list, most-frequent first. Pure + testable (#120). TLS contributes both a JA4
/// and a JA3 row when present; QUIC contributes an SNI row; SSH a HASSH row;
/// JA4H contributes one HTTP-client row (#124).
pub fn merge_fingerprints(
    tls: &[TlsRecord],
    quic: &[QuicRecord],
    ssh: &[SshRecord],
    ja4h: &[Ja4hRecord],
) -> Vec<Fingerprint> {
    let mut out: Vec<Fingerprint> = Vec::new();
    for t in tls {
        let sni = t.sni.clone();
        if let Some(ja4) = t.ja4.clone() {
            out.push(Fingerprint {
                kind: FpKind::Ja4,
                value: ja4,
                detail: sni.clone().unwrap_or_else(|| "-".into()),
                count: t.count,
                allowlist_host: sni.clone(),
            });
        }
        if let Some(ja3) = t.ja3.clone() {
            out.push(Fingerprint {
                kind: FpKind::Ja3,
                value: ja3,
                detail: sni.clone().unwrap_or_else(|| "-".into()),
                count: t.count,
                allowlist_host: sni.clone(),
            });
        }
    }
    for q in quic {
        let sni = q.sni.clone();
        out.push(Fingerprint {
            kind: FpKind::QuicSni,
            value: sni.clone().unwrap_or_else(|| "-".into()),
            detail: format!(
                "{} {}",
                q.version,
                if q.alpn.is_empty() {
                    String::new()
                } else {
                    q.alpn.join(",")
                }
            )
            .trim()
            .to_string(),
            count: q.count,
            allowlist_host: sni,
        });
    }
    for s in ssh {
        out.push(Fingerprint {
            kind: FpKind::Hassh,
            value: s.hassh.clone(),
            detail: format!(
                "{}{}",
                s.role,
                s.banner
                    .as_deref()
                    .map(|b| format!(" · {b}"))
                    .unwrap_or_default()
            ),
            count: s.count,
            allowlist_host: None,
        });
    }
    for h in ja4h {
        // Detail: method + Host (the request shape behind the fingerprint).
        let detail = match (h.method.as_deref(), h.host.as_deref()) {
            (Some(m), Some(host)) => format!("{m} {host}"),
            (Some(m), None) => m.to_string(),
            (None, Some(host)) => host.to_string(),
            (None, None) => "-".into(),
        };
        out.push(Fingerprint {
            kind: FpKind::Ja4h,
            value: h.ja4h.clone(),
            detail,
            count: h.count,
            // JA4H rows carry the Host header so they're allow-listable like SNI.
            allowlist_host: h.host.clone(),
        });
    }
    out.sort_by_key(|f| std::cmp::Reverse(f.count));
    out
}

/// Render the top-level inventory view (#120).
pub fn inventory_view(state: &InventoryState) -> Element<'_, Message> {
    let refresh = {
        let label = if state.loading {
            "Refreshing…"
        } else {
            "Refresh"
        };
        let mut b = button(text(label).size(font::CAPTION)).padding([4, 10]);
        if !state.loading {
            b = b.on_press(Message::OpenInventory);
        }
        b
    };

    let mut content = column![section_header("Inventory", Some(refresh.into()))].spacing(space::MD);

    if let Some(err) = &state.error {
        content = content.push(empty_state(format!("Fetch failed: {err}"), None));
    }

    content = content
        .push(card(render_assets(state)))
        .push(card(render_fingerprints(state)));

    container(scrollable(content.padding(space::LG)))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn render_assets(state: &InventoryState) -> Element<'_, Message> {
    let sort_pick = pick_list(
        AssetSort::ALL.as_slice(),
        Some(state.asset_sort),
        Message::SetInventoryAssetSort,
    )
    .width(Length::Fixed(130.0))
    .text_size(font::CAPTION);
    let header = row![
        section_header(format!("Assets ({})", state.assets.len()), None),
        text("sort:").size(font::CAPTION).style(dim),
        sort_pick,
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center);

    if state.assets.is_empty() {
        let hint = if state.loading {
            "Loading assets…"
        } else if !state.assets_responded {
            // A netring sensor may be running, but passive asset discovery is
            // opt-in (`collect.assets`, default off) so the queryable is absent.
            "Asset discovery is off on the netring sensor — set collect.assets = true in its config, then restart it"
        } else {
            // The sensor answered with an empty inventory: it's watching the wire
            // but hasn't observed any host yet (ARP/NDP/LLDP are passive).
            "No assets discovered yet — the netring sensor is listening for ARP/NDP/LLDP on the wire"
        };
        return column![header, empty_state(hint, None)]
            .spacing(space::SM)
            .into();
    }

    let mut list = Column::new().spacing(3).push(
        row![
            cell("mac", 150),
            cell("ip", 150),
            cell("hostname", 150),
            cell("vendor", 160),
            cell("platform", 150),
            cell("caps", 120),
            cell("seen via", 100),
            cell("last seen", 90),
        ]
        .spacing(8),
    );
    for r in state.sorted_assets().into_iter().take(500) {
        let ip = r
            .ipv4
            .first()
            .or_else(|| r.ipv6.first())
            .map(String::as_str)
            .unwrap_or("-");
        list = list.push(
            row![
                cell(&r.mac, 150),
                cell(ip, 150),
                cell(r.hostname.as_deref().unwrap_or("-"), 150),
                cell(r.vendor.as_deref().unwrap_or("-"), 160),
                cell(r.platform.as_deref().unwrap_or("-"), 150),
                cell(&join_or_dash(&r.capabilities), 120),
                cell(&join_or_dash(&r.seen_via), 100),
                cell(&format_timestamp(r.last_seen), 90),
            ]
            .spacing(8),
        );
    }
    column![header, list].spacing(space::SM).into()
}

fn render_fingerprints(state: &InventoryState) -> Element<'_, Message> {
    let header = section_header(
        format!("Fingerprint explorer ({})", state.fingerprints.len()),
        None,
    );

    // Kind filter chips: "all" + one per FpKind.
    let chip = |label: &str, active: bool, msg: Message| {
        let mut b = button(text(label.to_string()).size(font::CAPTION)).padding([2, 8]);
        b = b.on_press(msg);
        b = b.style(if active {
            iced::widget::button::primary
        } else {
            iced::widget::button::secondary
        });
        b
    };
    let mut chips = row![
        text("kind:").size(font::CAPTION).style(dim),
        chip(
            "all",
            state.fp_filter.is_none(),
            Message::SetInventoryFpFilter(None)
        ),
    ]
    .spacing(space::XS)
    .align_y(iced::Alignment::Center);
    for k in FpKind::ALL {
        chips = chips.push(chip(
            k.label(),
            state.fp_filter == Some(k),
            Message::SetInventoryFpFilter(Some(k)),
        ));
    }

    let rows = state.filtered_fingerprints();
    if rows.is_empty() {
        let hint = if state.loading {
            "Loading fingerprints…"
        } else {
            "No TLS/QUIC/SSH/HTTP fingerprints observed"
        };
        return column![header, chips, empty_state(hint, None)]
            .spacing(space::SM)
            .into();
    }

    let mut list = Column::new().spacing(3).push(
        row![
            cell("kind", 90),
            cell("fingerprint", 280),
            cell("context", 240),
            cell("count", 60),
            cell("", 90),
        ]
        .spacing(8),
    );
    for f in rows.into_iter().take(500) {
        // SNI-bearing rows can be added to the netring host allowlist (reusing the
        // detection-tuning command channel, #121); other rows are informational.
        let action: Element<'_, Message> = match &f.allowlist_host {
            Some(host) if !host.is_empty() && host != "-" => button(text("allowlist").size(10))
                .padding([2, 8])
                .on_press(Message::AddNetringAllowlistEntry(host.clone()))
                .into(),
            _ => cell("", 90),
        };
        list = list.push(
            row![
                cell(f.kind.label(), 90),
                cell(&f.value, 280),
                cell(&f.detail, 240),
                cell(&f.count.to_string(), 60),
                action,
            ]
            .spacing(8),
        );
    }
    column![header, chips, list].spacing(space::SM).into()
}

fn cell<'a>(s: &str, width: u16) -> Element<'a, Message> {
    text(s.to_string())
        .size(12)
        .width(Length::Fixed(width as f32))
        .into()
}

fn dim(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(theme).text_dimmed()),
    }
}

/// Join a slug list with commas, or `"-"` when empty.
fn join_or_dash(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_string()
    } else {
        items.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(mac: &str, vendor: Option<&str>, host: Option<&str>, last_seen: i64) -> AssetRecord {
        AssetRecord {
            mac: mac.into(),
            ipv4: vec![],
            ipv6: vec![],
            hostname: host.map(Into::into),
            vendor: vendor.map(Into::into),
            platform: None,
            capabilities: vec![],
            seen_via: vec![],
            last_seen,
        }
    }

    #[test]
    fn assets_sort_last_seen_desc() {
        let assets = vec![
            asset("a", None, None, 100),
            asset("b", None, None, 300),
            asset("c", None, None, 200),
        ];
        let sorted = sort_assets(&assets, AssetSort::LastSeen);
        assert_eq!(
            sorted.iter().map(|a| a.mac.as_str()).collect::<Vec<_>>(),
            vec!["b", "c", "a"]
        );
    }

    #[test]
    fn assets_sort_vendor_blanks_last() {
        let assets = vec![
            asset("a", Some("Zebra"), None, 1),
            asset("b", None, None, 1),
            asset("c", Some("acme"), None, 1),
        ];
        let sorted = sort_assets(&assets, AssetSort::Vendor);
        // case-insensitive ascending, blank vendor pushed last.
        assert_eq!(
            sorted.iter().map(|a| a.mac.as_str()).collect::<Vec<_>>(),
            vec!["c", "a", "b"]
        );
    }

    #[test]
    fn merge_fingerprints_groups_and_sorts() {
        let tls = vec![TlsRecord {
            sni: Some("example.com".into()),
            alpn: Some("h2".into()),
            ja3: Some("ja3hash".into()),
            ja4: Some("ja4hash".into()),
            count: 5,
        }];
        let quic = vec![QuicRecord {
            sni: Some("quic.example".into()),
            alpn: vec!["h3".into()],
            version: "v1".into(),
            count: 9,
        }];
        let ssh = vec![SshRecord {
            hassh: "hasshhash".into(),
            role: "client".into(),
            banner: Some("SSH-2.0-OpenSSH_9.6".into()),
            count: 1,
        }];
        let ja4h = vec![Ja4hRecord {
            ja4h: "ge11nn05enus_aaa".into(),
            host: Some("api.example".into()),
            method: Some("GET".into()),
            user_agent: Some("curl/8".into()),
            count: 7,
        }];
        let fps = merge_fingerprints(&tls, &quic, &ssh, &ja4h);
        // TLS yields a JA4 + JA3 row; plus QUIC + SSH + JA4H = 5 rows.
        assert_eq!(fps.len(), 5);
        // Sorted by count desc → QUIC (9) first, JA4H (7) second, SSH (1) last.
        assert_eq!(fps[0].kind, FpKind::QuicSni);
        assert_eq!(fps[0].count, 9);
        assert_eq!(fps[1].kind, FpKind::Ja4h);
        assert_eq!(fps[1].count, 7);
        assert_eq!(fps[1].detail, "GET api.example");
        assert_eq!(fps.last().unwrap().kind, FpKind::Hassh);
        // SNI/Host-bearing rows are allow-listable; HASSH is not.
        assert_eq!(fps[0].allowlist_host.as_deref(), Some("quic.example"));
        assert_eq!(fps[1].allowlist_host.as_deref(), Some("api.example"));
        assert!(fps.last().unwrap().allowlist_host.is_none());
    }

    #[test]
    fn fp_filter_by_kind() {
        let mut state = InventoryState::default();
        state.apply(Ok(InventoryData {
            assets: vec![],
            tls: vec![TlsRecord {
                sni: None,
                alpn: None,
                ja3: Some("j3".into()),
                ja4: Some("j4".into()),
                count: 1,
            }],
            quic: vec![],
            ssh: vec![],
            ja4h: vec![],
        }));
        assert_eq!(state.filtered_fingerprints().len(), 2);
        state.fp_filter = Some(FpKind::Ja4);
        assert_eq!(state.filtered_fingerprints().len(), 1);
        assert_eq!(state.filtered_fingerprints()[0].kind, FpKind::Ja4);
    }
}
