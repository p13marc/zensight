//! The Redfish client (#953) — read-only, bounded, and forgiving of the
//! resources a given firmware does not serve.
//!
//! Four deliberate choices, each of which the alternative gets wrong:
//!
//! - **Every response is `serde_json::Value` first**, then read field by
//!   field. Redfish is a large schema that vendors implement in patches: HPE,
//!   Dell, Lenovo and Supermicro disagree about which members exist, and every
//!   firmware release moves something. A strict struct would turn a cosmetic
//!   upstream difference into a sensor that reports nothing.
//! - **The surface is discovered, not assumed.** Redfish 2020.4 deprecated
//!   `Chassis/{id}/Power` and `Thermal` in favour of `PowerSubsystem` and
//!   `ThermalSubsystem`, and a great deal of shipped firmware serves only the
//!   old pair. The client tries the new one, falls back, and **records which
//!   answered** — because a field absent on one is a different fact from one
//!   absent on the other.
//! - **404 and 403 are data.** A chassis with no thermal resource is a fact
//!   about that hardware, not a failed poll; grading it as one would make a
//!   correctly-scoped read-only account look like a broken sensor. Logged once
//!   per transition, never once per cycle.
//! - **The client takes a full base URL string.** That is the seam that lets
//!   the e2e point it at a plain-HTTP fake: standing up a TLS listener would
//!   test rustls, not this sensor.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use zensight_common::bmc::{
    Chassis, Drive, Fan, Health, MemoryModule, PowerSupply, RedfishSurface, Redundancy,
    RedundancyGroup, State, ThermalSensor,
};

#[derive(Debug)]
pub enum ApiError {
    Transport(String),
    Status { code: u16, body: String },
    Malformed(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Transport(e) => write!(f, "transport: {e}"),
            ApiError::Status { code, body } => write!(f, "HTTP {code}: {body}"),
            ApiError::Malformed(e) => write!(f, "malformed reply: {e}"),
        }
    }
}

impl std::error::Error for ApiError {}

pub type Result<T> = std::result::Result<T, ApiError>;

/// Everything one chassis reported in one sweep.
#[derive(Debug, Clone)]
pub struct ChassisSweep {
    pub chassis: Chassis,
    pub supplies: Vec<PowerSupply>,
    pub fans: Vec<Fan>,
    pub thermal: Vec<ThermalSensor>,
    /// MACs the BMC knows about this machine, for the identity claim.
    /// Scoped to the systems **this chassis links** (#1110).
    pub macs: Vec<String>,
    /// The managed machine's own `ComputerSystem.HostName`, when it reports
    /// one (#1110). Deliberately **not** `Chassis.Name`, which is a schema
    /// description that ships as the literal "Computer System Chassis" on Dell,
    /// HPE and Supermicro — claiming it as a hostname asserts that every such
    /// machine is the same host. `None` means the BMC did not say, which is a
    /// missing claim rather than a wrong one.
    pub hostname: Option<String>,
    /// Physical drives behind the systems **this chassis links** (#1140),
    /// scoped the same way `macs` is and for the same reason.
    pub drives: Vec<Drive>,
    /// Memory modules behind the systems this chassis links (#1140).
    pub memory: Vec<MemoryModule>,
    /// Power and thermal redundancy groups as the *chassis* reports them
    /// (#1140), not as a member reports its own group.
    pub redundancy: Vec<RedundancyGroup>,
}

/// How many Redfish sessions this client will mint before it stops trying and
/// stays on basic auth for the rest of the process (#1140).
///
/// A firmware that hands out a token and then rejects it is worse than one
/// with no session service at all: every request costs a POST, a 401 and a
/// retry. Three is enough to ride out a reboot and few enough that a BMC
/// whose session table is full is not being filled further by us.
const MAX_SESSION_ATTEMPTS: u32 = 3;

/// How many times one GET is retried after a *transport* failure.
///
/// One. A BMC is a small embedded HTTP server and a slow iDRAC answers in one
/// to two seconds; a sweep at `max_concurrent: 4` against a 60 s interval has
/// no room for a third attempt, and a retry storm against a busy BMC is how
/// the sweep that was merely slow becomes the sweep that times out (#1140).
const MAX_GET_RETRIES: u32 = 1;

/// Consecutive transport failures before the client stops trying for
/// [`BREAKER_COOLDOWN`].
///
/// Without this, a BMC that has gone away costs a full sweep of timeouts every
/// interval — `max_concurrent` requests each waiting the whole `timeout`,
/// forever. The breaker turns that into one failed request per cooldown.
const BREAKER_THRESHOLD: u32 = 5;

/// How long the breaker stays open. Shorter than any sensible poll interval,
/// so a BMC that comes back is noticed on the next sweep rather than the one
/// after.
const BREAKER_COOLDOWN: Duration = Duration::from_secs(30);

/// How many pages of one Redfish collection are followed before the walk
/// stops.
///
/// The cursor is the device's, so it bounds a hostile or looping firmware as
/// well as a large one. Fifty pages at the smallest page size anyone ships is
/// far more members than a chassis has.
const MAX_COLLECTION_PAGES: usize = 50;

/// A minted Redfish session: the token to send and the resource to DELETE.
#[derive(Debug, Clone)]
struct Session {
    /// `X-Auth-Token`, sent on every request instead of basic auth.
    token: String,
    /// The `Location` the service returned, so the session can be given back.
    location: String,
}

/// What the client has learned about this firmware's session service.
#[derive(Debug, Default)]
struct SessionState {
    current: Option<Session>,
    /// Sessions minted so far, against [`MAX_SESSION_ATTEMPTS`].
    attempts: u32,
    /// Set once the service has told us it has none, or has refused its own
    /// tokens often enough. From here on it is basic auth, and that is a fact
    /// about the firmware rather than a failure.
    unsupported: bool,
}

/// Consecutive-failure state for the breaker.
#[derive(Debug, Default)]
struct Breaker {
    consecutive: u32,
    open_until: Option<Instant>,
}

pub struct RedfishClient {
    http: reqwest::Client,
    base: String,
    username: String,
    password: String,
    limit: Arc<Semaphore>,
    /// Paths currently answering 403/404/501, so a refusal is logged once per
    /// transition rather than once per poll (the #880 lesson).
    refused: Arc<Mutex<HashSet<String>>>,
    /// The session, if this firmware has one (#1140).
    ///
    /// An **async** mutex because minting a session is an `await` that has to
    /// happen under it: two concurrent sweeps must not both POST to
    /// `SessionService/Sessions`, which is precisely how "maximum number of
    /// sessions reached" is reached.
    session: Arc<AsyncMutex<SessionState>>,
    breaker: Arc<Mutex<Breaker>>,
}

impl RedfishClient {
    /// `base` is the full scheme+authority, e.g. `https://10.0.0.10` — or
    /// `http://127.0.0.1:PORT` in a test.
    pub fn new(
        base: String,
        username: String,
        password: String,
        timeout: Duration,
        insecure: bool,
        ca_pem: Option<Vec<u8>>,
        max_concurrent: usize,
    ) -> anyhow::Result<Self> {
        let mut builder = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("zensight-sensor-bmc/", env!("CARGO_PKG_VERSION")));
        if insecure {
            builder = builder.danger_accept_invalid_certs(true);
        }
        if let Some(pem) = ca_pem {
            // Added, not replaced: a BMC behind an internal CA still needs the
            // public roots for anything else the same client might reach.
            builder = builder.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
        }
        Ok(Self {
            http: builder.build()?,
            base,
            username,
            password,
            limit: Arc::new(Semaphore::new(max_concurrent.max(1))),
            refused: Arc::new(Mutex::new(HashSet::new())),
            session: Arc::new(AsyncMutex::new(SessionState::default())),
            breaker: Arc::new(Mutex::new(Breaker::default())),
        })
    }

    /// The `X-Auth-Token` to send, minting a session if this firmware has a
    /// session service and we have not given up on it (#1140).
    ///
    /// `None` means basic auth — either because the service does not serve
    /// `SessionService/Sessions`, or because we have spent
    /// [`MAX_SESSION_ATTEMPTS`].
    ///
    /// **Why this exists at all.** Sending `basic_auth` on every request is
    /// what the client used to do, and several firmwares — iDRAC and some
    /// Supermicro builds among them — mint a *session* per basic-auth request
    /// and never reap it. A sweep is dozens of requests; a few sweeps later the
    /// BMC answers "maximum number of sessions reached" to everything,
    /// including the operator's browser. The sensor that was only supposed to
    /// read then takes the management interface down, which is the one thing a
    /// read-only sensor must not do.
    async fn auth_token(&self) -> Option<String> {
        let mut state = self.session.lock().await;
        if let Some(s) = &state.current {
            return Some(s.token.clone());
        }
        if state.unsupported || state.attempts >= MAX_SESSION_ATTEMPTS {
            return None;
        }
        state.attempts += 1;
        let attempt = state.attempts;

        let resp = self
            .http
            .post(format!("{}/redfish/v1/SessionService/Sessions", self.base))
            .json(&serde_json::json!({
                "UserName": self.username,
                "Password": self.password,
            }))
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                // A transport failure here is the BMC being unreachable, not
                // the session service being absent. Do not mark it
                // unsupported — the request below will fail too and the
                // breaker will say so once.
                tracing::debug!(error = %e, "bmc: session POST failed; using basic auth this round");
                return None;
            }
        };

        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            // 404/405/501: no session service. 401: the credentials are wrong,
            // and basic auth will not do better — but saying so is the GET's
            // job, which reports the real status.
            if matches!(status, 400 | 401 | 403 | 404 | 405 | 500 | 501) {
                state.unsupported = true;
                tracing::info!(
                    status,
                    "bmc: no usable Redfish session service — falling back to basic auth for \
                     the life of this process"
                );
            }
            return None;
        }

        let token = resp
            .headers()
            .get("X-Auth-Token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        // Some firmware omits `Location` and puts the session's own
        // `@odata.id` in the body instead. Without one we cannot give the
        // session back, which is exactly the leak this is here to avoid — so
        // read the body too before settling for none.
        let body: Option<Value> = resp.json().await.ok();
        let location = location.or_else(|| {
            body.as_ref()
                .and_then(|b| b.get("@odata.id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });

        match (token, location) {
            (Some(token), Some(location)) => {
                tracing::info!(attempt, "bmc: Redfish session established");
                state.current = Some(Session {
                    token: token.clone(),
                    location,
                });
                Some(token)
            }
            (Some(token), None) => {
                // A token we cannot release is worse than no token: it is the
                // leak, one per process rather than one per request. Take it
                // for this process and say so, rather than re-minting.
                tracing::warn!(
                    "bmc: the session service returned a token with no Location and no \
                     `@odata.id` — it cannot be released on shutdown, so no further sessions \
                     will be minted"
                );
                state.unsupported = true;
                state.current = Some(Session {
                    token: token.clone(),
                    location: String::new(),
                });
                Some(token)
            }
            (None, _) => {
                state.unsupported = true;
                tracing::info!(
                    "bmc: the session service answered without an X-Auth-Token — basic auth"
                );
                None
            }
        }
    }

    /// Forget the current session so the next request mints a new one.
    async fn invalidate_session(&self) {
        let mut state = self.session.lock().await;
        if state.current.take().is_some() {
            tracing::debug!("bmc: session token rejected; it will be re-minted");
        }
    }

    /// Give the session back to the BMC.
    ///
    /// Called on shutdown. A session this process forgets is a session the
    /// firmware keeps until *its* timeout, and the session table is small —
    /// on some iDRAC builds eight entries for everything, the operator's
    /// browser included. Restarting this sensor eight times must not lock a
    /// human out of their own BMC.
    pub async fn close(&self) {
        let session = { self.session.lock().await.current.take() };
        let Some(session) = session else { return };
        if session.location.is_empty() {
            return;
        }
        let url = if session.location.starts_with("http") {
            session.location.clone()
        } else {
            format!("{}{}", self.base, session.location)
        };
        match self
            .http
            .delete(&url)
            .header("X-Auth-Token", &session.token)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                tracing::info!("bmc: Redfish session released")
            }
            Ok(r) => tracing::warn!(status = r.status().as_u16(), "bmc: session release refused"),
            Err(e) => tracing::warn!(error = %e, "bmc: session release failed"),
        }
    }

    /// `true` when the breaker is open and this request should not be sent.
    fn breaker_is_open(&self) -> bool {
        let mut b = self.breaker.lock().unwrap();
        match b.open_until {
            Some(until) if Instant::now() < until => true,
            Some(_) => {
                // Cooled down: let exactly one request through to find out.
                b.open_until = None;
                b.consecutive = 0;
                false
            }
            None => false,
        }
    }

    fn record_transport_failure(&self) {
        let mut b = self.breaker.lock().unwrap();
        b.consecutive += 1;
        if b.consecutive >= BREAKER_THRESHOLD && b.open_until.is_none() {
            b.open_until = Some(Instant::now() + BREAKER_COOLDOWN);
            tracing::warn!(
                consecutive = b.consecutive,
                cooldown_secs = BREAKER_COOLDOWN.as_secs(),
                "bmc: too many consecutive transport failures — pausing requests. A BMC that \
                 has gone away otherwise costs a whole sweep of timeouts every interval"
            );
        }
    }

    fn record_success(&self) {
        let mut b = self.breaker.lock().unwrap();
        b.consecutive = 0;
        b.open_until = None;
    }

    /// GET one Redfish resource. `Ok(None)` for 403/404/501 — a resource this
    /// firmware does not serve, or this account may not read.
    ///
    /// Authenticated with an `X-Auth-Token` when this firmware has a session
    /// service, and with basic auth otherwise (#1140). A 401 invalidates the
    /// session and is retried once with a fresh one: a BMC that rebooted
    /// between sweeps should cost one extra request, not a failed sweep.
    pub async fn get(&self, path: &str) -> Result<Option<Value>> {
        if self.breaker_is_open() {
            return Err(ApiError::Transport(
                "breaker open: too many consecutive failures, waiting for the cooldown".to_string(),
            ));
        }

        let mut transport_retries = 0;
        let mut reauthed = false;
        loop {
            let permit = self
                .limit
                .acquire()
                .await
                .map_err(|e| ApiError::Transport(e.to_string()))?;

            let token = self.auth_token().await;
            let mut req = self.http.get(format!("{}{}", self.base, path));
            req = match &token {
                Some(t) => req.header("X-Auth-Token", t),
                None => req.basic_auth(&self.username, Some(&self.password)),
            };

            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    drop(permit);
                    if transport_retries < MAX_GET_RETRIES {
                        transport_retries += 1;
                        continue;
                    }
                    self.record_transport_failure();
                    return Err(ApiError::Transport(e.to_string()));
                }
            };
            self.record_success();
            let status = resp.status();

            // A rejected token is not an authorization verdict about the
            // resource: the session expired, or the BMC rebooted. Re-mint once
            // and ask again, so the whole sweep does not fail for it.
            if status.as_u16() == 401 && token.is_some() && !reauthed {
                drop(permit);
                reauthed = true;
                self.invalidate_session().await;
                continue;
            }

            if matches!(status.as_u16(), 403 | 404 | 501) {
                if self.refused.lock().unwrap().insert(path.to_string()) {
                    tracing::warn!(
                        path = %path,
                        status = status.as_u16(),
                        "bmc: this Redfish resource is not served here — what it carries will \
                         be reported as unknown, never as zero"
                    );
                }
                return Ok(None);
            }
            if self.refused.lock().unwrap().remove(path) {
                tracing::info!(path = %path, "bmc: resource readable again");
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(ApiError::Status {
                    code: status.as_u16(),
                    // Never echo an unbounded body into a log line.
                    body: body.chars().take(200).collect(),
                });
            }
            return resp
                .json::<Value>()
                .await
                .map(Some)
                .map_err(|e| ApiError::Malformed(e.to_string()));
        }
    }

    /// The chassis ids this service exposes.
    pub async fn chassis_ids(&self) -> Result<Vec<String>> {
        // Paginated (#1140): a blade enclosure is exactly the shape that runs
        // past a firmware's page size, and losing the tail here loses whole
        // machines rather than a few sensors.
        Ok(self
            .collection_links("/redfish/v1/Chassis")
            .await?
            .into_iter()
            .filter_map(|link| link.rsplit('/').find(|s| !s.is_empty()).map(str::to_string))
            .collect())
    }

    /// Poll one chassis end to end.
    pub async fn sweep(&self, id: &str) -> Result<ChassisSweep> {
        let root = self
            .get(&format!("/redfish/v1/Chassis/{id}"))
            .await?
            .unwrap_or(Value::Null);

        // The modern surface first, then the legacy pair. Which one answered
        // is recorded: a reading absent on one is a different fact from the
        // same reading absent on the other.
        let mut groups: Vec<RedundancyGroup> = Vec::new();
        let (mut supplies, mut fans, mut thermal, surface) = {
            let power_sub = self
                .get(&format!("/redfish/v1/Chassis/{id}/PowerSubsystem"))
                .await?;
            let thermal_sub = self
                .get(&format!("/redfish/v1/Chassis/{id}/ThermalSubsystem"))
                .await?;
            // The GROUP's own view of its redundancy (#1140). Read here, off
            // the subsystem body that is already in hand, rather than inferred
            // from a member's copy of it.
            groups.extend(parse_redundancy_groups(power_sub.as_ref(), "power"));
            groups.extend(parse_redundancy_groups(thermal_sub.as_ref(), "thermal"));
            if power_sub.is_some() || thermal_sub.is_some() {
                let supplies = self
                    .collection(&format!(
                        "/redfish/v1/Chassis/{id}/PowerSubsystem/PowerSupplies"
                    ))
                    .await?;
                let fans = self
                    .collection(&format!("/redfish/v1/Chassis/{id}/ThermalSubsystem/Fans"))
                    .await?;
                // ThermalMetrics is a SINGLETON, not a collection (#1131).
                // It has no `Members`; it carries `TemperatureReadingsCelsius`
                // — an array of sensor excerpts — directly on the body. Read
                // through `collection()` it yielded nothing, every time, so
                // this sensor published no temperature at all on the modern
                // surface while the legacy arm below worked correctly. A BMC
                // old enough to serve only `Chassis/{id}/Thermal` reported
                // temperatures and a new one did not, which is the inversion
                // that kept it hidden.
                let sensors = thermal_readings(
                    self.get(&format!(
                        "/redfish/v1/Chassis/{id}/ThermalSubsystem/ThermalMetrics"
                    ))
                    .await?
                    .as_ref(),
                );
                (
                    supplies.iter().map(parse_supply).collect::<Vec<_>>(),
                    fans.iter().map(parse_fan).collect::<Vec<_>>(),
                    sensors,
                    RedfishSurface::Subsystem,
                )
            } else {
                let power = self.get(&format!("/redfish/v1/Chassis/{id}/Power")).await?;
                let therm = self
                    .get(&format!("/redfish/v1/Chassis/{id}/Thermal"))
                    .await?;
                // The legacy pair carries `Redundancy` on the same bodies
                // (#1140) — a great deal of shipped firmware serves only this
                // surface, and its groups are no less real.
                groups.extend(parse_redundancy_groups(power.as_ref(), "power"));
                groups.extend(parse_redundancy_groups(therm.as_ref(), "thermal"));
                let surface = if power.is_some() || therm.is_some() {
                    RedfishSurface::Legacy
                } else {
                    RedfishSurface::None
                };
                let supplies = array(power.as_ref(), "PowerSupplies")
                    .iter()
                    .map(parse_supply)
                    .collect::<Vec<_>>();
                let fans = array(therm.as_ref(), "Fans")
                    .iter()
                    .map(parse_fan)
                    .collect::<Vec<_>>();
                let thermal = array(therm.as_ref(), "Temperatures")
                    .iter()
                    .map(parse_thermal)
                    .collect::<Vec<_>>();
                (supplies, fans, thermal, surface)
            }
        };

        // Ids have to be stable and unique, or two bays take turns overwriting
        // one document. Anything unnamed falls back to its position.
        fill_ids(&mut supplies, |s| &mut s.id);
        fill_ids(&mut fans, |f| &mut f.id);
        fill_ids(&mut thermal, |t| &mut t.id);

        fill_ids(&mut groups, |g| &mut g.id);

        let chassis = parse_chassis(id, &root, surface);
        let (macs, hostname) = self.identity(&root).await.unwrap_or_default();
        // Best-effort, like `identity`: a firmware that serves no Storage or
        // Memory collection yields none, and none is a missing reading rather
        // than a wrong one. A failure here must not cost the fans.
        let (mut drives, mut memory) = self.inventory(&root).await.unwrap_or_default();
        fill_ids(&mut drives, |d| &mut d.id);
        fill_ids(&mut memory, |m| &mut m.id);

        Ok(ChassisSweep {
            chassis,
            supplies,
            fans,
            thermal,
            macs,
            hostname,
            drives,
            memory,
            redundancy: groups,
        })
    }

    /// Drives and memory modules behind the systems **this chassis links**
    /// (#1140).
    ///
    /// Scoped through `Chassis/{id}/Links/ComputerSystems`, exactly as
    /// `identity` is and for the same reason (#1110): one Redfish service
    /// fronts several machines on a blade enclosure or a four-node Twin, and
    /// walking `/redfish/v1/Systems` would put every node's DIMMs on every
    /// chassis.
    ///
    /// The failure this closes: a drive or a DIMM the BMC has already marked
    /// `Warning` rolled up into `Chassis.Status.Health` and nowhere else, so
    /// `chassis-health` fired saying "the BMC reports a fault" and named
    /// nothing an operator could act on.
    async fn inventory(&self, chassis_root: &Value) -> Result<(Vec<Drive>, Vec<MemoryModule>)> {
        let mut drives = Vec::new();
        let mut memory = Vec::new();

        for system in linked_systems(chassis_root) {
            // Storage is two levels: controllers, then each controller's
            // `Drives` — which is an inline array of links, not a paginated
            // collection, so it is read as one.
            for ctrl_link in self
                .collection_links(&format!("{system}/Storage"))
                .await
                .unwrap_or_default()
            {
                let Ok(Some(ctrl)) = self.get(&ctrl_link).await else {
                    continue;
                };
                let ctrl_id = text(&ctrl, "Id").or_else(|| {
                    ctrl_link
                        .rsplit('/')
                        .find(|s| !s.is_empty())
                        .map(str::to_string)
                });
                for drive_link in ctrl
                    .get("Drives")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|d| d.get("@odata.id").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
                {
                    if let Ok(Some(d)) = self.get(&drive_link).await {
                        drives.push(parse_drive(&d, ctrl_id.as_deref()));
                    }
                }
            }

            for dimm_link in self
                .collection_links(&format!("{system}/Memory"))
                .await
                .unwrap_or_default()
            {
                if let Ok(Some(m)) = self.get(&dimm_link).await {
                    memory.push(parse_memory(&m));
                }
            }
        }
        Ok((drives, memory))
    }

    /// Every member link of a Redfish collection, **across every page**
    /// (#1140).
    ///
    /// Redfish paginates a collection with `Members@odata.nextLink`, and this
    /// client read one page. A firmware with a page size smaller than the
    /// chassis — 50 is common, and a populated blade enclosure or a machine
    /// with dozens of thermal sensors passes it — silently lost the tail. Not
    /// an error anywhere: a shorter list, which reads as fewer fans.
    ///
    /// Bounded twice over, because the cursor comes from the device:
    /// [`MAX_COLLECTION_PAGES`] pages, and a page whose `nextLink` repeats one
    /// already followed ends the walk rather than looping forever.
    async fn collection_links(&self, path: &str) -> Result<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut next = Some(path.to_string());
        let mut pages = 0;

        while let Some(page) = next.take() {
            if !seen.insert(page.clone()) {
                tracing::warn!(
                    path = %path,
                    repeated = %page,
                    "bmc: the collection's nextLink points at a page already read — stopping \
                     rather than looping"
                );
                break;
            }
            pages += 1;
            if pages > MAX_COLLECTION_PAGES {
                tracing::warn!(
                    path = %path,
                    cap = MAX_COLLECTION_PAGES,
                    members = out.len(),
                    "bmc: collection page cap reached; the tail is not read"
                );
                break;
            }
            let Some(body) = self.get(&page).await? else {
                break;
            };
            out.extend(members(&body));
            next = body
                .get("Members@odata.nextLink")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        Ok(out)
    }

    /// Every member of a Redfish collection, fetched. An unreadable member is
    /// skipped rather than failing the sweep: one bad bay must not cost the
    /// other seven.
    async fn collection(&self, path: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        for link in self.collection_links(path).await? {
            if let Ok(Some(member)) = self.get(&link).await {
                out.push(member);
            }
        }
        Ok(out)
    }

    /// What the BMC knows about the machine (or machines) in **this** chassis,
    /// for the identity claim.
    ///
    /// Scoped through `Chassis/{id}/Links/ComputerSystems` (#1110). It used to
    /// ignore its chassis argument entirely and walk every member of
    /// `/redfish/v1/Systems`, so on a 4-node Twin or a blade enclosure — one
    /// Redfish service in front of several machines — the union of all nodes'
    /// MACs landed on *each* chassis's evidence. MAC is the catalog's strongest
    /// merge key after `host_id`, so that claim asks it to fuse every node in
    /// the enclosure into one host.
    ///
    /// A chassis that links no system yields nothing, which is correct: this
    /// is a claim about a machine, and a chassis with no machine in it has none
    /// to make.
    ///
    /// Best-effort throughout: absent on plenty of firmware, and absent is
    /// fine — it is a *missing* claim, not a wrong one.
    async fn identity(&self, chassis_root: &Value) -> Result<(Vec<String>, Option<String>)> {
        let mut macs = Vec::new();
        let mut hostname = None;
        for system in linked_systems(chassis_root) {
            let Ok(Some(sys)) = self.get(&system).await else {
                continue;
            };
            // The machine's OWN name, as it knows it. Not `Chassis.Name`,
            // which is a schema description — Dell, HPE and Supermicro all
            // ship the literal "Computer System Chassis", so claiming it as a
            // hostname asserts that every such machine is the same host.
            if hostname.is_none()
                && let Some(h) = sys.get("HostName").and_then(Value::as_str)
                && !h.trim().is_empty()
            {
                hostname = Some(h.to_string());
            }
            let Ok(ifaces) = self
                .collection_links(&format!("{system}/EthernetInterfaces"))
                .await
            else {
                continue;
            };
            for iface in ifaces {
                if let Ok(Some(nic)) = self.get(&iface).await
                    && let Some(mac) = nic.get("MACAddress").and_then(Value::as_str)
                    && !mac.is_empty()
                {
                    macs.push(mac.to_ascii_lowercase());
                }
            }
        }
        macs.sort();
        macs.dedup();
        Ok((macs, hostname))
    }
}

// ── parsing ─────────────────────────────────────────────────────────────────
//
// Free functions over `Value`, so every shape below is testable against a
// fixture without a socket — which is most of what there is to get wrong.

/// The `ComputerSystem` links a chassis declares (#1110).
///
/// `Chassis/{id}/Links/ComputerSystems` is Redfish's own statement of which
/// machines are in this enclosure, and it is the difference between one
/// blade's identity claim and the whole chassis's.
pub fn linked_systems(chassis: &Value) -> Vec<String> {
    chassis
        .get("Links")
        .and_then(|l| l.get("ComputerSystems"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("@odata.id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `@odata.id` links out of a Redfish collection.
pub fn members(body: &Value) -> Vec<String> {
    body.get("Members")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("@odata.id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The temperature excerpts on a `ThermalMetrics` **singleton** (#1131).
///
/// Redfish puts these on the body as `TemperatureReadingsCelsius`, not behind
/// a `Members` collection — the mistake this exists to make impossible to
/// repeat. `TemperatureSummaryCelsius` is accepted as the older spelling some
/// firmware still serves.
///
/// The excerpts are a reduced shape (`DeviceName`/`Reading`, no `Status`), and
/// `parse_thermal` already tolerates it: it tries `Reading` beside
/// `ReadingCelsius`, and `MemberId` beside `Id`. `DeviceName` is mapped onto
/// `Name` here so the series is labelled with something an operator recognises
/// rather than an empty string.
pub fn thermal_readings(body: Option<&Value>) -> Vec<ThermalSensor> {
    let Some(body) = body else {
        return Vec::new();
    };
    let readings = body
        .get("TemperatureReadingsCelsius")
        .or_else(|| body.get("TemperatureSummaryCelsius"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    readings
        .iter()
        .map(|r| {
            let mut sensor = parse_thermal(r);
            if sensor.name.is_none() {
                sensor.name = text(r, "DeviceName").or_else(|| text(r, "DataSourceUri"));
            }
            sensor
        })
        .collect()
}

/// A legacy embedded array (`Power.PowerSupplies`, `Thermal.Fans`).
fn array(body: Option<&Value>, key: &str) -> Vec<Value> {
    body.and_then(|b| b.get(key))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn status(v: &Value) -> (Health, State) {
    let s = v.get("Status");
    let health = s
        .and_then(|s| s.get("Health"))
        .and_then(|h| serde_json::from_value(h.clone()).ok())
        .unwrap_or(Health::Unknown);
    let state = s
        .and_then(|s| s.get("State"))
        .and_then(|h| serde_json::from_value(h.clone()).ok())
        .unwrap_or(State::Unknown);
    (health, state)
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A number from any of several member names, in preference order.
///
/// Redfish moved several readings between releases (`PowerInputWatts` is the
/// modern spelling of `PowerInputWatts`/`LastPowerOutputWatts`), and a vendor
/// may serve either. Absent from all of them stays absent.
fn number(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| {
        v.get(*k).and_then(|n| {
            n.as_f64()
                .or_else(|| n.get("Reading").and_then(Value::as_f64))
        })
    })
}

fn redundancy(v: &Value) -> (Option<String>, Option<Redundancy>) {
    let group = text(v, "RedundancyGroup").or_else(|| {
        v.get("Redundancy")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|r| text(r, "Name"))
    });
    let status_value = v
        .get("Redundancy")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned();
    let verdict = status_value.as_ref().map(|r| {
        let (health, _) = status(r);
        match health {
            Health::OK => Redundancy::Full,
            Health::Warning => Redundancy::Degraded,
            _ => Redundancy::Failed,
        }
    });
    (group, verdict)
}

pub fn parse_supply(v: &Value) -> PowerSupply {
    let (health, state) = status(v);
    let (redundancy_group, redundancy) = redundancy(v);
    let present = state.is_present();
    PowerSupply {
        id: text(v, "Id")
            .or_else(|| text(v, "MemberId"))
            .unwrap_or_default(),
        name: text(v, "Name"),
        present,
        health,
        state,
        // An empty bay reports no watts. Publishing 0 would read as a supply
        // drawing nothing, which is a different and wrong statement.
        input_watts: present
            .then(|| number(v, &["PowerInputWatts", "LineInputVoltage_W"]))
            .flatten(),
        output_watts: present
            .then(|| number(v, &["PowerOutputWatts", "LastPowerOutputWatts"]))
            .flatten(),
        capacity_watts: number(v, &["PowerCapacityWatts", "CapacityWatts"]),
        redundancy_group,
        redundancy,
        model: text(v, "Model"),
        serial: text(v, "SerialNumber"),
    }
}

pub fn parse_fan(v: &Value) -> Fan {
    let (health, state) = status(v);
    let (redundancy_group, redundancy) = redundancy(v);
    let present = state.is_present();
    Fan {
        id: text(v, "Id")
            .or_else(|| text(v, "MemberId"))
            .unwrap_or_default(),
        name: text(v, "Name").or_else(|| text(v, "FanName")),
        present,
        health,
        state,
        // `SpeedPercent` is deliberately NOT read: it is a percentage of
        // maximum, a different quantity, and publishing it on a series named
        // `rpm` would be a wrong number rather than a missing one (#954).
        rpm: present
            .then(|| number(v, &["SpeedRPM", "Reading", "ReadingRPM"]))
            .flatten(),
        redundancy_group,
        redundancy,
    }
}

pub fn parse_thermal(v: &Value) -> ThermalSensor {
    let (health, state) = status(v);
    ThermalSensor {
        id: text(v, "Id")
            .or_else(|| text(v, "MemberId"))
            .unwrap_or_default(),
        name: text(v, "Name"),
        health,
        state,
        celsius: number(v, &["ReadingCelsius", "Reading", "TemperatureCelsius"]),
        upper_critical_c: number(v, &["UpperThresholdCritical", "ReadingRangeMax"]),
        upper_warning_c: number(v, &["UpperThresholdNonCritical"]),
    }
}

pub fn parse_chassis(id: &str, v: &Value, surface: RedfishSurface) -> Chassis {
    let (health, state) = status(v);
    Chassis {
        id: id.to_string(),
        name: text(v, "Name"),
        manufacturer: text(v, "Manufacturer"),
        model: text(v, "Model"),
        serial: text(v, "SerialNumber"),
        asset_tag: text(v, "AssetTag"),
        power_state: text(v, "PowerState"),
        intrusion: v
            .get("PhysicalSecurity")
            .and_then(|p| text(p, "IntrusionSensor")),
        health,
        state,
        firmware: text(v, "FirmwareVersion"),
        surface,
    }
}

/// Give anything unnamed a stable id from its position.
///
/// Redfish's legacy embedded arrays often omit `MemberId`, and an empty id
/// would collapse every bay onto one key — eight supplies taking turns
/// overwriting one document, which reads as a chassis that keeps changing its
/// mind.
/// An unsigned integer from any of several member names, in preference order.
fn uint(v: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| v.get(*k).and_then(Value::as_u64))
}

/// One physical drive (#1140).
pub fn parse_drive(v: &Value, controller: Option<&str>) -> Drive {
    let (health, state) = status(v);
    let present = state.is_present();
    Drive {
        id: text(v, "Id")
            .or_else(|| text(v, "MemberId"))
            .unwrap_or_default(),
        name: text(v, "Name"),
        controller: controller.map(str::to_string),
        present,
        health,
        state,
        model: text(v, "Model"),
        serial: text(v, "SerialNumber"),
        media_type: text(v, "MediaType"),
        protocol: text(v, "Protocol"),
        // An empty bay reports no capacity. Publishing 0 would read as a
        // zero-byte drive, which is a different and wrong statement — the same
        // rule every reading in this file follows.
        capacity_bytes: present.then(|| uint(v, &["CapacityBytes"])).flatten(),
        life_left_percent: present
            .then(|| number(v, &["PredictedMediaLifeLeftPercent"]))
            .flatten(),
        // `None` is "the BMC did not say", which is not "no failure
        // predicted". A spinning disk on firmware that passes no SMART
        // summary through must not read as healthy-and-checked.
        failure_predicted: v.get("FailurePredicted").and_then(Value::as_bool),
    }
}

/// One memory module (#1140).
pub fn parse_memory(v: &Value) -> MemoryModule {
    let (health, state) = status(v);
    // A Redfish `Memory` resource for an empty slot is served with
    // `Status.State: Absent` — and some firmware serves it with no `Status` at
    // all and `CapacityMiB: 0`. Both mean the slot is empty, and a slot with
    // no DIMM in it is not a 0 GiB DIMM.
    let capacity = uint(v, &["CapacityMiB"]);
    let present = state.is_present() && capacity != Some(0);
    MemoryModule {
        id: text(v, "Id")
            .or_else(|| text(v, "MemberId"))
            .unwrap_or_default(),
        name: text(v, "Name").or_else(|| text(v, "DeviceLocator")),
        present,
        health,
        state,
        capacity_mib: present.then_some(capacity).flatten(),
        device_type: text(v, "MemoryDeviceType"),
        manufacturer: text(v, "Manufacturer"),
        serial: text(v, "SerialNumber"),
        speed_mhz: present
            .then(|| uint(v, &["OperatingSpeedMhz", "AllowedSpeedsMHz"]))
            .flatten(),
    }
}

/// The `Redundancy` array a subsystem carries, as groups (#1140).
///
/// `subsystem` is `power` or `thermal`: two subsystems can number their groups
/// from zero, and a key that did not say which would have them overwrite each
/// other.
pub fn parse_redundancy_groups(body: Option<&Value>, subsystem: &str) -> Vec<RedundancyGroup> {
    array(body, "Redundancy")
        .iter()
        .map(|r| {
            let (health, state) = status(r);
            RedundancyGroup {
                id: text(r, "Id")
                    .or_else(|| text(r, "MemberId"))
                    .unwrap_or_default(),
                name: text(r, "Name"),
                subsystem: subsystem.to_string(),
                health,
                state,
                // The GROUP's verdict, read from the group. The per-member
                // copy this sensor used to rely on is a member's opinion of
                // its own group, and a group below `MinNumNeeded` while every
                // surviving member still says Full is exactly what that
                // could not see.
                redundancy: Some(match health {
                    Health::OK => Redundancy::Full,
                    Health::Warning => Redundancy::Degraded,
                    _ => Redundancy::Failed,
                }),
                min_needed: uint(r, &["MinNumNeeded"]).map(|n| n as u32),
                max_supported: uint(r, &["MaxNumSupported"]).map(|n| n as u32),
                members: r
                    .get("RedundancySet")
                    .and_then(Value::as_array)
                    .map(|a| a.len() as u32),
            }
        })
        .collect()
}

fn fill_ids<T>(items: &mut [T], id: impl Fn(&mut T) -> &mut String) {
    for (i, item) in items.iter_mut().enumerate() {
        let slot = id(item);
        if slot.is_empty() {
            *slot = i.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// #1110: a chassis claims the systems **it links**, not every system the
    /// Redfish service knows.
    ///
    /// On a 4-node Twin or a blade enclosure one service fronts several
    /// machines, and `macs()` ignored its chassis argument and walked
    /// `/redfish/v1/Systems` wholesale — so the union of every node's MACs
    /// landed on *each* chassis's evidence. MAC is the catalog's strongest
    /// merge key after `host_id`, so that claim asks it to fuse the whole
    /// enclosure into one host.
    #[test]
    fn a_chassis_links_its_own_systems_only() {
        let blade_a = json!({
            "Id": "1",
            "Links": { "ComputerSystems": [{"@odata.id": "/redfish/v1/Systems/1"}] },
        });
        let blade_b = json!({
            "Id": "2",
            "Links": { "ComputerSystems": [{"@odata.id": "/redfish/v1/Systems/2"}] },
        });
        assert_eq!(linked_systems(&blade_a), vec!["/redfish/v1/Systems/1"]);
        assert_eq!(linked_systems(&blade_b), vec!["/redfish/v1/Systems/2"]);
        assert!(
            linked_systems(&blade_a)
                .iter()
                .all(|s| !linked_systems(&blade_b).contains(s)),
            "two blades must not claim each other's systems"
        );
    }

    /// A chassis that links no system makes no identity claim, which is
    /// correct: a chassis with no machine in it has no machine to describe.
    /// Firmware that omits `Links` entirely lands here too — a *missing* claim
    /// rather than a wrong one.
    #[test]
    fn a_chassis_with_no_linked_system_claims_nothing() {
        assert!(linked_systems(&json!({"Id": "1"})).is_empty());
        assert!(linked_systems(&json!({"Id": "1", "Links": {}})).is_empty());
        assert!(linked_systems(&json!({"Id": "1", "Links": {"ComputerSystems": []}})).is_empty());
    }

    /// An enclosure that fronts several machines links them all, and then the
    /// union is correct rather than a merge hazard.
    #[test]
    fn a_multi_system_chassis_links_all_of_them() {
        let enclosure = json!({
            "Id": "encl",
            "Links": { "ComputerSystems": [
                {"@odata.id": "/redfish/v1/Systems/1"},
                {"@odata.id": "/redfish/v1/Systems/2"},
            ]},
        });
        assert_eq!(linked_systems(&enclosure).len(), 2);
    }

    /// A healthy modern supply.
    #[test]
    fn a_present_supply_carries_its_watts() {
        let s = parse_supply(&json!({
            "Id": "0",
            "Name": "PSU 1",
            "Status": {"Health": "OK", "State": "Enabled"},
            "PowerInputWatts": 210.5,
            "PowerOutputWatts": 190.0,
            "PowerCapacityWatts": 800.0,
            "Model": "PS-2801",
            "SerialNumber": "SN123",
        }));
        assert!(s.present);
        assert_eq!(s.health, Health::OK);
        assert_eq!(s.input_watts, Some(210.5));
        assert_eq!(s.capacity_watts, Some(800.0));
    }

    /// An empty bay publishes NO watts. Zero would read as a supply drawing
    /// nothing, which is a different and wrong statement — and the one an
    /// operator would act on.
    #[test]
    fn an_absent_bay_reports_no_watts_rather_than_zero() {
        let s = parse_supply(&json!({
            "Id": "1",
            "Status": {"State": "Absent"},
            // Some firmware leaves stale readings in an absent slot.
            "PowerInputWatts": 0,
            "PowerOutputWatts": 0,
        }));
        assert!(!s.present);
        assert_eq!(s.input_watts, None, "an absent bay measures nothing");
        assert_eq!(s.output_watts, None);
    }

    /// A fan's speed as a percentage of maximum is a different quantity from
    /// RPM. Publishing it as `rpm` would be a wrong number, not a missing one.
    #[test]
    fn a_percentage_fan_speed_is_not_an_rpm() {
        let f = parse_fan(&json!({
            "Id": "0",
            "Status": {"Health": "OK", "State": "Enabled"},
            "SpeedPercent": {"Reading": 42.0},
        }));
        assert_eq!(f.rpm, None, "SpeedPercent must not become rpm");

        let f = parse_fan(&json!({
            "Id": "0",
            "Status": {"Health": "OK", "State": "Enabled"},
            "SpeedRPM": 4800,
        }));
        assert_eq!(f.rpm, Some(4800.0));
    }

    /// A fan at zero RPM that the BMC calls Critical is a failed fan, and the
    /// zero is a real reading — the one case where zero IS the measurement.
    #[test]
    fn a_stopped_fan_keeps_its_zero() {
        let f = parse_fan(&json!({
            "Id": "3",
            "Status": {"Health": "Critical", "State": "Enabled"},
            "Reading": 0,
        }));
        assert_eq!(f.rpm, Some(0.0));
        assert!(f.health.is_faulted());
    }

    /// The legacy and modern spellings of the same reading both land.
    #[test]
    fn both_redfish_generations_of_a_temperature_parse() {
        let legacy = parse_thermal(&json!({
            "MemberId": "0",
            "Name": "Inlet Temp",
            "Status": {"Health": "OK", "State": "Enabled"},
            "ReadingCelsius": 23.0,
            "UpperThresholdCritical": 45.0,
            "UpperThresholdNonCritical": 40.0,
        }));
        assert_eq!(legacy.celsius, Some(23.0));
        assert_eq!(legacy.upper_critical_c, Some(45.0));
        assert_eq!(legacy.over_critical(), Some(false));

        let modern = parse_thermal(&json!({
            "Id": "cpu1",
            "Status": {"Health": "Critical", "State": "Enabled"},
            "Reading": 96.0,
            "UpperThresholdCritical": 90.0,
        }));
        assert_eq!(modern.over_critical(), Some(true));
    }

    /// Redfish's legacy arrays often omit an id. Without a fallback every bay
    /// collapses onto one key and the chassis reads as if it kept changing
    /// its mind.
    #[test]
    fn unnamed_members_get_stable_positional_ids() {
        let mut fans: Vec<Fan> = vec![
            parse_fan(&json!({"Status": {"State": "Enabled"}, "Reading": 1000})),
            parse_fan(&json!({"Status": {"State": "Enabled"}, "Reading": 2000})),
        ];
        fill_ids(&mut fans, |f| &mut f.id);
        assert_eq!(fans[0].id, "0");
        assert_eq!(fans[1].id, "1");
    }

    /// #1131: `ThermalMetrics` is a singleton. Read as a collection it yielded
    /// nothing on every BMC serving the modern surface, so the sensor whose
    /// whole purpose is "a physical fault the host cannot see" published no
    /// temperature at all — silently, because an empty list is what a chassis
    /// with no sensors also looks like.
    ///
    /// This body is the shape Redfish actually serves: NO `Members`.
    #[test]
    fn thermal_metrics_is_a_singleton_not_a_collection() {
        let body = json!({
            "@odata.id": "/redfish/v1/Chassis/1/ThermalSubsystem/ThermalMetrics",
            "Id": "ThermalMetrics",
            "TemperatureReadingsCelsius": [
                {"DeviceName": "CPU1 Temp", "Reading": 47.0, "MemberId": "0"},
                {"DeviceName": "Inlet Temp", "Reading": 21.5, "MemberId": "1"},
            ],
        });

        // The old path: no `Members`, so a collection read finds nothing.
        assert!(
            members(&body).is_empty(),
            "if this ever grows a Members array the singleton premise is wrong"
        );

        let sensors = thermal_readings(Some(&body));
        assert_eq!(sensors.len(), 2, "both readings must survive");
        assert_eq!(sensors[0].name.as_deref(), Some("CPU1 Temp"));
        assert_eq!(sensors[0].celsius, Some(47.0));
        assert_eq!(sensors[1].name.as_deref(), Some("Inlet Temp"));
        assert_eq!(sensors[1].celsius, Some(21.5));
        assert_eq!(sensors[1].id, "1", "MemberId keys the series");
    }

    /// A chassis that really has no thermal readings, and a BMC that does not
    /// serve the resource at all, are both an empty list — not a panic and not
    /// a fabricated zero.
    #[test]
    fn an_absent_or_empty_thermal_metrics_is_no_readings() {
        assert!(thermal_readings(None).is_empty());
        assert!(thermal_readings(Some(&json!({"Id": "ThermalMetrics"}))).is_empty());
        assert!(thermal_readings(Some(&json!({"TemperatureReadingsCelsius": []}))).is_empty());
    }

    /// Some firmware serves the older `TemperatureSummaryCelsius` spelling.
    #[test]
    fn the_older_temperature_summary_spelling_is_read_too() {
        let body = json!({
            "TemperatureSummaryCelsius": [{"DeviceName": "Exhaust", "Reading": 33.0}],
        });
        let sensors = thermal_readings(Some(&body));
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].name.as_deref(), Some("Exhaust"));
        assert_eq!(sensors[0].celsius, Some(33.0));
    }

    #[test]
    fn collection_members_are_read_as_links() {
        let body = json!({"Members": [
            {"@odata.id": "/redfish/v1/Chassis/1"},
            {"@odata.id": "/redfish/v1/Chassis/2"},
        ]});
        assert_eq!(
            members(&body),
            vec!["/redfish/v1/Chassis/1", "/redfish/v1/Chassis/2"]
        );
        assert!(members(&json!({})).is_empty());
    }

    /// Which surface answered is part of the document, because a reading
    /// absent on the legacy pair is a different fact from one absent on the
    /// modern one.
    #[test]
    fn the_chassis_records_which_surface_answered() {
        let c = parse_chassis(
            "1",
            &json!({
                "Name": "Computer System Chassis",
                "Manufacturer": "ACME",
                "SerialNumber": "CH-1",
                "PowerState": "On",
                "PhysicalSecurity": {"IntrusionSensor": "Normal"},
                "Status": {"Health": "OK", "State": "Enabled"},
            }),
            RedfishSurface::Legacy,
        );
        assert_eq!(c.surface, RedfishSurface::Legacy);
        assert_eq!(c.power_state.as_deref(), Some("On"));
        assert_eq!(c.intrusion.as_deref(), Some("Normal"));
    }

    /// A BMC that answers with a document missing every optional member must
    /// still parse. A strict struct here would turn a thin firmware into a
    /// sensor that reports nothing.
    #[test]
    fn a_nearly_empty_document_still_parses() {
        let c = parse_chassis("1", &json!({}), RedfishSurface::None);
        assert_eq!(c.health, Health::Unknown);
        assert_eq!(c.state, State::Unknown);
        assert!(c.name.is_none());

        let s = parse_supply(&json!({}));
        assert_eq!(s.input_watts, None);
        let t = parse_thermal(&json!({}));
        assert_eq!(t.over_critical(), None);
    }
}
