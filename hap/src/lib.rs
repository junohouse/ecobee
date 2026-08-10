//! Ecobee thermostats and SmartSensors over HomeKit, on the LAN.
//!
//! The same hardware `ecobee.thermostat` reaches through ecobee's cloud, reached instead over
//! the protocol the thermostat already speaks to your own network. It keeps working when the
//! internet does not, and there is no rate limit to tiptoe around.
//!
//! Two manifests share this module: `ecobee.hap.thermostat` (the accessory — pairing lives
//! here) and `ecobee.hap.sensor` (a SmartSensor the thermostat bridges). A sensor is never set
//! up on its own; it is found by browsing the thermostat, the way a Hue bulb is found by
//! browsing its bridge.
//!
//! None of the cryptography is here. HAP is a *transport*, so it lives in core behind
//! `hap-pair://` (pair once with the 8-digit code) and `hap://` (everything after) — this
//! driver only ever makes what look like ordinary HTTP requests.
//!
//! # Reading the accessory database
//!
//! `GET /accessories` returns every accessory, every service, and every characteristic *with
//! its current value*, in one response. So there is no separate read step and no table of
//! characteristic ids to keep in sync: one request answers the thermostat and all of its
//! sensors at once. Writing does need an id, and those are remembered from the last read.
//!
//! Nothing below matches on an accessory number. The thermostat is "the accessory with a
//! Thermostat service"; a sensor is the aid its setup flow recorded. Ecobee is free to
//! renumber, and a firmware update that does will not silently point us at the wrong room.

use driver_sdk::*;
use std::collections::BTreeMap;

const THERMOSTAT: &str = "ecobee.hap.thermostat";
const SENSOR: &str = "ecobee.hap.sensor";

#[derive(Default)]
pub struct EcobeeHap;

// HAP service and characteristic types. Accessories report the short form ("4A"), but the
// specification writes them as full UUIDs, and some firmwares send those — `hap_type`
// normalises both to the same thing.
const SRV_THERMOSTAT: &str = "4A";
const SRV_TEMPERATURE: &str = "8A";
const SRV_OCCUPANCY: &str = "86";

const CH_CURRENT_STATE: &str = "F";
const CH_TARGET_STATE: &str = "33";
const CH_CURRENT_TEMP: &str = "11";
const CH_TARGET_TEMP: &str = "35";
const CH_COOL_THRESHOLD: &str = "D";
const CH_HEAT_THRESHOLD: &str = "12";
const CH_CURRENT_HUMIDITY: &str = "10";
const CH_OCCUPANCY: &str = "71";
const CH_NAME: &str = "23";
/// `TemperatureDisplayUnits`: 0 Celsius, 1 Fahrenheit. What the wall shows.
const CH_DISPLAY_UNITS: &str = "36";

/// `0000004A-0000-1000-8000-0026BB765291`, `004A`, and `4A` are the same characteristic.
fn hap_type(raw: &str) -> String {
    let head = raw.split('-').next().unwrap_or(raw);
    let trimmed = head.trim_start_matches('0').to_uppercase();
    if trimmed.is_empty() { "0".into() } else { trimmed }
}

// ---------------------------------------------------------------------------------------
// Reading the accessory database
// ---------------------------------------------------------------------------------------

/// One characteristic, flattened out of the nesting that carried it.
struct Characteristic {
    aid: u64,
    iid: u64,
    ty: String,
    value: Value,
}

fn accessories(doc: &Value) -> Vec<&Value> {
    doc.get("accessories")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

fn services(accessory: &Value) -> Vec<&Value> {
    accessory
        .get("services")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

fn aid_of(accessory: &Value) -> u64 {
    accessory.get("aid").and_then(Value::as_u64).unwrap_or(0)
}

fn has_service(accessory: &Value, want: &str) -> bool {
    services(accessory)
        .iter()
        .any(|s| s.get("type").and_then(Value::as_str).map(hap_type).as_deref() == Some(want))
}

/// Every characteristic of one service on one accessory.
fn characteristics(accessory: &Value, service: &Value) -> Vec<Characteristic> {
    let aid = aid_of(accessory);
    service
        .get("characteristics")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|c| {
                    Some(Characteristic {
                        aid,
                        iid: c.get("iid").and_then(Value::as_u64)?,
                        ty: hap_type(c.get("type").and_then(Value::as_str)?),
                        value: c.get("value").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn find<'a>(chars: &'a [Characteristic], ty: &str) -> Option<&'a Characteristic> {
    chars.iter().find(|c| c.ty == ty)
}

fn number(chars: &[Characteristic], ty: &str) -> Option<f64> {
    find(chars, ty).and_then(|c| c.value.as_f64())
}

/// A service's own `Name` characteristic — what the installer called it in the ecobee app,
/// which is a far better label than "Sensor 2".
fn name_of(chars: &[Characteristic]) -> Option<String> {
    find(chars, CH_NAME)
        .and_then(|c| c.value.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Round to one decimal. HomeKit reports tenths; float noise past that is not a temperature
/// change and should not look like one.
fn tenths(c: f64) -> f64 {
    (c * 10.0).round() / 10.0
}

/// HomeKit's heating/cooling state numbers, in the words the thermostat proxy uses.
fn mode_name(v: f64) -> &'static str {
    match v as i64 {
        1 => "heat",
        2 => "cool",
        3 => "auto",
        _ => "off",
    }
}

fn mode_value(name: &str) -> Option<u8> {
    match name {
        "off" => Some(0),
        "heat" => Some(1),
        "cool" => Some(2),
        "auto" => Some(3),
        _ => None,
    }
}

/// What the equipment is doing *right now*, as opposed to what it has been told to do.
fn hvac_state(v: f64) -> &'static str {
    match v as i64 {
        1 => "heating",
        2 => "cooling",
        _ => "idle",
    }
}

// ---------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------

/// Attach the stored pairing to a request. These headers are read by core's `hap` transport
/// and never reach a socket — same arrangement as the LEAP driver's client certificate.
fn authorised(inst: &Instance, path: &str, method: &str) -> Option<HttpRequest> {
    let address = text(inst.property("Address"))?;
    let port = inst.property("Port").as_u64().unwrap_or(80);
    Some(
        HttpRequest::new(method, format!("hap://{address}:{port}{path}"))
            .header("x-hap-accessory-id", text(inst.property("Accessory id"))?)
            .header("x-hap-accessory-ltpk", text(inst.property("Accessory key"))?)
            .header("x-hap-controller-id", text(inst.property("Controller id"))?)
            .header("x-hap-controller-sk", text(inst.property("Controller key"))?),
    )
}

fn text(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

/// Write characteristics. HomeKit takes several in one request, which is what makes a
/// dual-setpoint change one write rather than two that momentarily disagree.
fn write(inst: &Instance, values: Vec<Value>) -> Vec<HostCall> {
    let Some(req) = authorised(inst, "/characteristics", "PUT") else {
        return vec![HostCall::warn(
            "ecobee-hap: this thermostat is not paired — run its setup again",
        )];
    };
    let body = json!({ "characteristics": values });
    vec![HostCall::Http(req.json(body.to_string()))]
}

/// Where a characteristic lives, remembered from the last read so a write knows its id.
fn remembered(inst: &Instance, key: &str) -> Option<(u64, u64)> {
    let entry = inst.scratch.get("iids")?.get(key)?;
    Some((
        entry.get(0)?.as_u64()?,
        entry.get(1)?.as_u64()?,
    ))
}

fn remember(inst: &mut Instance, key: &str, c: Option<&Characteristic>) {
    let Some(c) = c else { return };
    let map = inst
        .scratch
        .entry("iids".to_string())
        .or_insert_with(|| json!({}));
    if let Some(obj) = map.as_object_mut() {
        obj.insert(key.to_string(), json!([c.aid, c.iid]));
    }
}

fn target(inst: &Instance, key: &str, value: Value) -> Option<Value> {
    let (aid, iid) = remembered(inst, key)?;
    Some(json!({ "aid": aid, "iid": iid, "value": value }))
}

// ---------------------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------------------

impl DriverModule for EcobeeHap {
    fn on_bind(&self, inst: &mut Instance) -> Vec<HostCall> {
        // Sensors ask too, rather than waiting to be handed the thermostat's answer: a
        // response goes to the device that asked for it and is not fanned out to children, so
        // a sensor that stayed quiet would hold its startup reading forever — and a sensor
        // stuck at "clear" looks exactly like a room nobody has walked into.
        let Some(req) = authorised(inst, "/accessories", "GET") else {
            return vec![HostCall::warn(
                "ecobee-hap: set this thermostat up through its HomeKit pairing flow first",
            )];
        };
        vec![HostCall::Http(req)]
    }

    fn on_command(
        &self,
        inst: &mut Instance,
        _proxy: LocalId,
        cmd: &str,
        args: &Args,
    ) -> Vec<HostCall> {
        // Only the thermostat proxy takes commands. A sensor is read-only, and the proxy
        // contract already refuses anything else before it reaches here.
        if inst.property("Accessory aid").as_u64().is_some() {
            return vec![HostCall::warn(format!(
                "ecobee-hap: a room sensor is read-only, got `{cmd}`"
            ))];
        }
        if inst.scratch.get("iids").is_none() {
            return vec![HostCall::warn(
                "ecobee-hap: nothing read from the thermostat yet — it will accept commands \
                 once its first poll comes back",
            )];
        }

        match cmd {
            "set_mode" => {
                let Some(mode) = args.get("mode").and_then(Value::as_str) else {
                    return vec![HostCall::warn("ecobee-hap: no mode")];
                };
                let Some(v) = mode_value(mode) else {
                    return vec![HostCall::warn(format!("ecobee-hap: unknown mode `{mode}`"))];
                };
                let Some(t) = target(inst, "target_state", json!(v)) else {
                    return vec![HostCall::warn("ecobee-hap: no target state characteristic")];
                };
                inst.scratch.insert("mode".into(), json!(mode));
                let mut a = Args::new();
                a.insert("mode".into(), json!(mode));
                let mut out = write(inst, vec![t]);
                out.push(HostCall::notify(1, "mode_changed", a));
                out
            }

            "set_heat_setpoint" | "set_cool_setpoint" => {
                let Some(c) = args.get("celsius").and_then(Value::as_f64) else {
                    return vec![HostCall::warn("ecobee-hap: no celsius value")];
                };
                let heating = cmd == "set_heat_setpoint";

                // In auto the thermostat holds a band, and HomeKit keeps that band in two
                // threshold characteristics; `TargetTemperature` is simply ignored. In a
                // single-setpoint mode it is the other way round. Writing the wrong one is
                // accepted and then silently does nothing, which is the worst way to fail.
                let auto = inst.scratch.get("mode").and_then(Value::as_str) == Some("auto");
                let values = if auto {
                    let key = if heating { "heat_threshold" } else { "cool_threshold" };
                    let Some(t) = target(inst, key, json!(c)) else {
                        return vec![HostCall::warn(format!(
                            "ecobee-hap: this thermostat has no {key} characteristic"
                        ))];
                    };
                    vec![t]
                } else {
                    let Some(t) = target(inst, "target_temp", json!(c)) else {
                        return vec![HostCall::warn("ecobee-hap: no target temperature")];
                    };
                    vec![t]
                };

                let mut a = Args::new();
                a.insert("which".into(), json!(if heating { "heat" } else { "cool" }));
                a.insert("celsius".into(), json!(c));
                let mut out = write(inst, values);
                out.push(HostCall::notify(1, "setpoint_changed", a));
                // `setpoint_changed` carries which setpoint in a sibling field, so the state
                // key it updates cannot be inferred from a parameter name.
                // Mirror the read side: in auto the band moved, otherwise the single
                // setpoint did.
                out.push(HostCall::SetState {
                    proxy: 1,
                    key: if !auto {
                        "setpoint_c"
                    } else if heating {
                        "heat_setpoint_c"
                    } else {
                        "cool_setpoint_c"
                    }
                    .into(),
                    value: json!(c),
                });
                out
            }

            other => vec![HostCall::warn(format!("ecobee-hap: unhandled `{other}`"))],
        }
    }

    /// A read came back. One document describes the thermostat and every sensor it bridges, so
    /// each device picks out its own part of it.
    fn on_event(
        &self,
        inst: &mut Instance,
        _control: LocalId,
        note: &str,
        args: &Args,
    ) -> Vec<HostCall> {
        if note != "http_response" {
            return Vec::new();
        }
        let Some(doc) = args.get("body") else {
            return Vec::new();
        };
        // A write is answered with 204 and no body; there is nothing to read from it.
        if accessories(doc).is_empty() {
            return Vec::new();
        }

        match inst.property("Accessory aid").as_u64() {
            Some(aid) => sensor_report(inst, doc, aid),
            None => thermostat_report(inst, doc),
        }
    }

    fn discover(&self, driver_id: &str, state: &Value, input: &Args) -> (SetupStep, Value) {
        if driver_id != THERMOSTAT {
            // Sensors are found by browsing the thermostat, never set up on their own.
            return (SetupStep::done(Vec::new()), Value::Null);
        }
        setup::run(state, input)
    }
}

/// The thermostat's own reading: the Thermostat service, plus whatever sensors sit on the same
/// accessory rather than on a bridged one.
fn thermostat_report(inst: &mut Instance, doc: &Value) -> Vec<HostCall> {
    let list = accessories(doc);
    let Some(accessory) = list.iter().find(|a| has_service(a, SRV_THERMOSTAT)) else {
        return vec![HostCall::warn(
            "ecobee-hap: nothing at this address has a thermostat — is it the right accessory?",
        )];
    };

    let mut out = vec![online(1)];

    for service in services(accessory) {
        let ty = service
            .get("type")
            .and_then(Value::as_str)
            .map(hap_type)
            .unwrap_or_default();
        let chars = characteristics(accessory, service);

        match ty.as_str() {
            SRV_THERMOSTAT => {
                remember(inst, "target_state", find(&chars, CH_TARGET_STATE));
                remember(inst, "target_temp", find(&chars, CH_TARGET_TEMP));
                remember(inst, "heat_threshold", find(&chars, CH_HEAT_THRESHOLD));
                remember(inst, "cool_threshold", find(&chars, CH_COOL_THRESHOLD));

                if let Some(c) = number(&chars, CH_CURRENT_TEMP) {
                    out.push(notify_f(1, "temperature_changed", "celsius", tenths(c)));
                }
                if let Some(h) = number(&chars, CH_CURRENT_HUMIDITY) {
                    out.push(notify_f(1, "humidity_changed", "percent", h));
                }
                if let Some(m) = number(&chars, CH_TARGET_STATE) {
                    let name = mode_name(m);
                    inst.scratch.insert("mode".into(), json!(name));
                    let mut a = Args::new();
                    a.insert("mode".into(), json!(name));
                    out.push(HostCall::notify(1, "mode_changed", a));
                }
                if let Some(s) = number(&chars, CH_CURRENT_STATE) {
                    let mut a = Args::new();
                    a.insert("state".into(), json!(hvac_state(s)));
                    out.push(HostCall::notify(1, "hvac_state_changed", a));
                }

                // In auto the two thresholds are the setpoints; otherwise both sides of the
                // proxy's pair follow the single target, which is what the thermostat is
                // actually holding.
                // What the thermostat displays. Read, never assumed: a unit is a property of
                // the device in front of someone, not of the protocol.
                if let Some(u) = number(&chars, CH_DISPLAY_UNITS) {
                    out.push(HostCall::SetState {
                        proxy: 1,
                        key: "display_unit".into(),
                        value: json!(if u as i64 == 1 { "°F" } else { "°C" }),
                    });
                }

                // In auto the thermostat holds a band and both thresholds are real. In heat or
                // cool it holds one number, and reporting a heat *and* a cool setpoint would
                // invent a second one nobody set — so only the one in force is reported.
                let mode = number(&chars, CH_TARGET_STATE).map(|m| m as i64);
                let set = |key: &str, v: Option<f64>| {
                    v.map(|v| HostCall::SetState {
                        proxy: 1,
                        key: key.into(),
                        value: json!(tenths(v)),
                    })
                };
                match mode {
                    Some(3) => {
                        out.extend(set("heat_setpoint_c", number(&chars, CH_HEAT_THRESHOLD)));
                        out.extend(set("cool_setpoint_c", number(&chars, CH_COOL_THRESHOLD)));
                    }
                    // Off holds nothing, so there is no setpoint to report at all.
                    Some(0) | None => {}
                    _ => out.extend(set("setpoint_c", number(&chars, CH_TARGET_TEMP))),
                }
            }
            SRV_OCCUPANCY => {
                if let Some(v) = number(&chars, CH_OCCUPANCY) {
                    out.extend(occupancy(inst, 2, "built_in", v > 0.0));
                }
            }
            SRV_TEMPERATURE => {
                if let Some(c) = number(&chars, CH_CURRENT_TEMP) {
                    out.push(notify_f(3, "value_changed", "value", tenths(c)));
                }
            }
            _ => {}
        }
    }
    out
}

/// One bridged SmartSensor's reading.
fn sensor_report(inst: &mut Instance, doc: &Value, aid: u64) -> Vec<HostCall> {
    let list = accessories(doc);
    let Some(accessory) = list.iter().find(|a| aid_of(a) == aid) else {
        // The sensor is out of range or has been removed in the ecobee app. Reporting a stale
        // reading would be worse than reporting nothing.
        return vec![offline(1)];
    };

    let mut out = vec![online(1)];
    for service in services(accessory) {
        let ty = service
            .get("type")
            .and_then(Value::as_str)
            .map(hap_type)
            .unwrap_or_default();
        let chars = characteristics(accessory, service);
        match ty.as_str() {
            SRV_OCCUPANCY => {
                if let Some(v) = number(&chars, CH_OCCUPANCY) {
                    out.extend(occupancy(inst, 1, "occupied", v > 0.0));
                }
            }
            SRV_TEMPERATURE => {
                if let Some(c) = number(&chars, CH_CURRENT_TEMP) {
                    out.push(notify_f(2, "value_changed", "value", tenths(c)));
                }
            }
            _ => {}
        }
    }
    out
}

/// Report occupancy only when it changed. A poll every thirty seconds would otherwise
/// re-announce "still detected" forever, and every one of those would re-trigger a rule.
fn occupancy(inst: &mut Instance, proxy: LocalId, key: &str, detected: bool) -> Vec<HostCall> {
    if inst.scratch.get(key).and_then(Value::as_bool) == Some(detected) {
        return Vec::new();
    }
    inst.scratch.insert(key.to_string(), json!(detected));
    let mut a = Args::new();
    a.insert("detected".into(), json!(detected));
    vec![HostCall::notify(proxy, "detected_changed", a)]
}

fn notify_f(proxy: LocalId, name: &str, param: &str, value: f64) -> HostCall {
    let mut a = Args::new();
    a.insert(param.into(), json!(value));
    HostCall::notify(proxy, name, a)
}

fn online(proxy: LocalId) -> HostCall {
    let mut a = Args::new();
    a.insert("online".into(), json!(true));
    HostCall::notify(proxy, "online_changed", a)
}

fn offline(proxy: LocalId) -> HostCall {
    let mut a = Args::new();
    a.insert("online".into(), json!(false));
    HostCall::notify(proxy, "online_changed", a)
}

// ---------------------------------------------------------------------------------------
// Setup — pair once with the code on the thermostat, then list what it bridges.
// ---------------------------------------------------------------------------------------

mod setup {
    use super::*;

    fn field(state: &Value, key: &str) -> String {
        state.get(key).and_then(Value::as_str).unwrap_or("").to_string()
    }

    fn instruct(title: &str, body: &str) -> SetupStep {
        SetupStep::Instruct {
            title: title.into(),
            body: body.into(),
            continue_label: "Continue".into(),
        }
    }

    fn with(state: &Value, updates: &[(&str, Value)]) -> Value {
        let mut m = state.as_object().cloned().unwrap_or_default();
        for (k, v) in updates {
            m.insert((*k).to_string(), v.clone());
        }
        Value::Object(m)
    }

    /// What core's mDNS scan found. HomeKit accessories advertise their port, and a `sf` of 0
    /// in the TXT record means the accessory is already paired to something — almost always
    /// Apple Home, and the single most common reason pairing fails.
    #[derive(Clone)]
    struct Candidate2 {
        name: String,
        address: String,
        port: u64,
        paired_already: bool,
        /// The `md` TXT field — the accessory's own model name.
        model: String,
    }

    impl Candidate2 {
        /// What an ecobee puts in `md` is not its marketing name and not the part number on
        /// the box. A real Smart Thermostat on the bench announces `ECB701`; the product pages
        /// would have you match `ecobee…` or `EB-STATE…`, and neither hits. Older units do
        /// report `ecobee3`/`ecobee4`, so all three prefixes stay.
        ///
        /// ponytail: prefixes, not a model table. `ecb` is short enough to collide with some
        /// unrelated vendor eventually, and a false positive costs only a pre-filled address
        /// the installer still reads before typing a code. A false negative hides the very
        /// thermostat someone is trying to add, which is how this was found.
        fn is_ecobee(&self) -> bool {
            let m = self.model.to_lowercase();
            m.starts_with("ecobee") || m.starts_with("eb-") || m.starts_with("ecb")
        }
    }

    fn found(state: &Value) -> Vec<Candidate2> {
        let all: Vec<Candidate2> = state
            .get("mdns_candidates")
            .and_then(Value::as_array)
            .map(|v| {
                v.iter()
                    .filter_map(|c| {
                        Some(Candidate2 {
                            name: c.get("name")?.as_str()?.to_string(),
                            address: c.get("address")?.as_str()?.to_string(),
                            port: c.get("port").and_then(Value::as_u64).unwrap_or(80),
                            paired_already: c
                                .pointer("/txt/sf")
                                .and_then(Value::as_str)
                                .is_some_and(|sf| sf.trim() == "0"),
                            model: c
                                .pointer("/txt/md")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        // `_hap._tcp` is every HomeKit accessory in the house, not every ecobee — a Caséta
        // bridge and a Nanoleaf panel answer the same query. Offering one of those as the
        // default here would spend a pairing slot on the wrong device, and HomeKit only
        // allows one controller, so undoing that means a factory reset.
        let ecobees: Vec<Candidate2> = all.iter().filter(|c| c.is_ecobee()).cloned().collect();
        if !ecobees.is_empty() {
            return ecobees;
        }
        // Nothing identified itself as an ecobee. Everything that answered is still worth
        // showing — "there is a HomeKit accessory at .102" is useful — but `ask` never turns
        // one of these into a default, so nothing gets paired by simply pressing continue.
        all
    }

    pub fn run(state: &Value, input: &Args) -> (SetupStep, Value) {
        // Browsing a thermostat that is paired already — core seeded the state with its
        // properties, so go straight to reading what it bridges.
        if state.get("browse").and_then(Value::as_bool) == Some(true) {
            if input.get("response").is_some() || input.get("error").is_some() {
                return list(state, input, false);
            }
            return read_accessories(state);
        }

        match field(state, "stage").as_str() {
            "pairing" => after_pairing(state, input),
            "listing" => list(state, input, true),
            _ => ask(state, input),
        }
    }

    fn ask(state: &Value, input: &Args) -> (SetupStep, Value) {
        let discovered = found(state);
        let confirmed = discovered.iter().find(|c| c.is_ecobee());

        // Address and port are not asked for. A HomeKit accessory picks a fresh port every
        // time it opens a pairing window — this one went 38399 then 40937 — so a typed port is
        // wrong by the time it is submitted, and a typed address only ever gets someone into
        // the position of pairing the wrong accessory. If the thermostat is not announcing
        // itself, the answer is to go and make it announce itself, not to describe it.
        let Some(target) = confirmed else {
            return (nothing_found(&discovered), state.clone());
        };
        if target.paired_already {
            return (
                instruct(
                    "Already paired",
                    &format!(
                        "{} is paired to another home. HomeKit allows one at a time, so remove \
                         it there — or on the thermostat, Settings › HomeKit › reset the \
                         pairing — then continue.",
                        short(&target.name)
                    ),
                ),
                state.clone(),
            );
        }

        let code = input
            .get("Setup code")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if !code.is_empty() {
            let (address, port) = (target.address.clone(), target.port);
            return pair(state, &address, port, &code);
        }

        (
            SetupStep::Form {
                title: "Pair an ecobee over HomeKit".into(),
                body: format!(
                    "Found {} at {}. Enter the setup code showing on the thermostat under \
                     Settings › HomeKit.",
                    short(&target.name),
                    target.address
                ),
                fields: vec![Field {
                    name: "Setup code".into(),
                    label: "HomeKit setup code".into(),
                    kind: "password".into(),
                    help: "8 digits, like 123-45-678".into(),
                    default: None,
                    options: Vec::new(),
                    required: true,
                }],
            },
            state.clone(),
        )
    }

    /// No ecobee is announcing itself. Which is nearly always the same situation — the
    /// thermostat only runs its HomeKit server while the pairing screen is up — so say the one
    /// thing that fixes it rather than offering a form that cannot help.
    fn nothing_found(discovered: &[Candidate2]) -> SetupStep {
        let others = if discovered.is_empty() {
            String::new()
        } else {
            format!(
                " ({} answered, but {} not an ecobee.)",
                discovered
                    .iter()
                    .map(|c| short(&c.name))
                    .collect::<Vec<_>>()
                    .join(", "),
                if discovered.len() == 1 { "it is" } else { "they are" }
            )
        };
        instruct(
            "Put the thermostat into pairing mode",
            &format!(
                "No ecobee is announcing itself.{others}\n\n\
                 On the thermostat: Main Menu › Settings › HomeKit. Leave the setup code on \
                 screen — it stops advertising once you leave that screen — then continue.",
            ),
        )
    }

    /// `ecobee._hap._tcp.local.` reads better as `ecobee`.
    fn short(fullname: &str) -> String {
        fullname.split('.').next().unwrap_or(fullname).to_string()
    }

    fn pair(state: &Value, address: &str, port: u64, code: &str) -> (SetupStep, Value) {
        let request = HttpRequest::new("POST", format!("hap-pair://{address}:{port}/"))
            .header("x-hap-code", code);
        (
            SetupStep::Fetch { request, note: "pair".into() },
            with(
                state,
                &[
                    ("stage", json!("pairing")),
                    ("address", json!(address)),
                    ("port", json!(port)),
                ],
            ),
        )
    }

    fn after_pairing(state: &Value, input: &Args) -> (SetupStep, Value) {
        if let Some(err) = input.get("error").and_then(Value::as_str) {
            return (
                SetupStep::Failed {
                    reason: format!("Pairing failed: {err}"),
                },
                Value::Null,
            );
        }
        let response = input.get("response").cloned().unwrap_or(Value::Null);
        let get = |k: &str| response.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let (acc_id, acc_key) = (get("accessory_id"), get("accessory_ltpk"));
        if acc_id.is_empty() || acc_key.is_empty() {
            return (
                SetupStep::Failed {
                    reason: format!("The thermostat did not complete pairing: {response}"),
                },
                Value::Null,
            );
        }
        read_accessories(&with(
            state,
            &[
                ("stage", json!("listing")),
                ("Accessory id", json!(acc_id)),
                ("Accessory key", json!(acc_key)),
                ("Controller id", json!(get("controller_id"))),
                ("Controller key", json!(get("controller_sk"))),
            ],
        ))
    }

    fn read_accessories(state: &Value) -> (SetupStep, Value) {
        // Browsing an existing thermostat arrives with the device's own property names; a
        // fresh pairing has just written the same values under those names too.
        let address = pick(state, &["Address", "address"]);
        let port = state
            .get("Port")
            .or_else(|| state.get("port"))
            .and_then(Value::as_u64)
            .unwrap_or(80);

        let request = HttpRequest::new("GET", format!("hap://{address}:{port}/accessories"))
            .header("x-hap-accessory-id", pick(state, &["Accessory id"]))
            .header("x-hap-accessory-ltpk", pick(state, &["Accessory key"]))
            .header("x-hap-controller-id", pick(state, &["Controller id"]))
            .header("x-hap-controller-sk", pick(state, &["Controller key"]));
        (
            SetupStep::Fetch { request, note: "list".into() },
            with(state, &[("stage", json!("listing"))]),
        )
    }

    fn pick(state: &Value, keys: &[&str]) -> String {
        keys.iter()
            .map(|k| field(state, k))
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    }

    /// Turn the accessory database into things to adopt: the thermostat itself, and one
    /// candidate per SmartSensor it bridges.
    fn list(state: &Value, input: &Args, include_thermostat: bool) -> (SetupStep, Value) {
        if let Some(err) = input.get("error").and_then(Value::as_str) {
            return (
                SetupStep::Failed {
                    reason: format!("Paired, but could not read the thermostat: {err}"),
                },
                Value::Null,
            );
        }
        let doc = input.get("response").cloned().unwrap_or(Value::Null);
        let list = accessories(&doc);
        if list.is_empty() {
            return (
                SetupStep::Failed {
                    reason: format!("The thermostat answered with nothing to set up: {doc}"),
                },
                Value::Null,
            );
        }

        let address = pick(state, &["Address", "address"]);
        let port = state
            .get("Port")
            .or_else(|| state.get("port"))
            .and_then(Value::as_u64)
            .unwrap_or(80);

        let mut out = Vec::new();
        if include_thermostat {
            let mut props = BTreeMap::new();
            props.insert("Address".into(), json!(address));
            props.insert("Port".into(), json!(port));
            for key in ["Accessory id", "Accessory key", "Controller id", "Controller key"] {
                props.insert(key.into(), json!(field(state, key)));
            }
            out.push(Candidate {
                label: thermostat_name(&list).unwrap_or_else(|| "Ecobee Thermostat".into()),
                kind: "thermostat".into(),
                driver_id: THERMOSTAT.into(),
                properties: props,
                verified: "paired over HomeKit".into(),
                            ..Default::default()
            });
        }

        for accessory in &list {
            // The thermostat's own accessory is not a room sensor, however many sensor
            // services it happens to carry.
            if has_service(accessory, SRV_THERMOSTAT) {
                continue;
            }
            if !has_service(accessory, SRV_OCCUPANCY) && !has_service(accessory, SRV_TEMPERATURE) {
                continue;
            }
            let aid = aid_of(accessory);
            let mut props = BTreeMap::new();
            props.insert("Accessory aid".into(), json!(aid));
            out.push(Candidate {
                label: accessory_name(accessory).unwrap_or_else(|| format!("Ecobee Sensor {aid}")),
                kind: "sensor".into(),
                driver_id: SENSOR.into(),
                properties: props,
                verified: "bridged by the thermostat".into(),
                            ..Default::default()
            });
        }

        (SetupStep::done(out), Value::Null)
    }

    fn thermostat_name(list: &[&Value]) -> Option<String> {
        let accessory = list.iter().find(|a| has_service(a, SRV_THERMOSTAT))?;
        accessory_name(accessory)
    }

    /// Whatever the accessory calls itself — the name set in the ecobee app, which is what the
    /// person setting this up already thinks of the device as.
    fn accessory_name(accessory: &Value) -> Option<String> {
        services(accessory)
            .iter()
            .find_map(|s| name_of(&characteristics(accessory, s)))
    }
}

export_driver!(EcobeeHap);
