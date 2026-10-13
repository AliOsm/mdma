// Server-rendered views.
use super::upstream::Status;
use mdma::grpc::{FederationTopology, Host, MemorySnapshot, MemoryUsage, ModelStatus};
use mdma::registry::parameters_label;
use rama::http::protocols::html::*;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum View {
    #[default]
    Memory,
    Federation,
    Playground,
}

impl View {
    const ALL: [Self; 3] = [Self::Memory, Self::Federation, Self::Playground];

    /// (key, path, label)
    const fn names(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Memory => ("memory", "/", "Memory & placement"),
            Self::Federation => ("federation", "/federation", "Federation"),
            Self::Playground => ("playground", "/playground", "Playground"),
        }
    }

    pub const fn key(self) -> &'static str {
        self.names().0
    }

    const fn path(self) -> &'static str {
        self.names().1
    }

    const fn label(self) -> &'static str {
        self.names().2
    }
}

/// The page's signals; chat starts on when the default model has a chat template.
fn signals(status: &Status) -> String {
    format!(
        "{{prompt: '', model: '', chat: {}, system: 'You are a helpful assistant.', max_tokens: 128, temperature: '', \
         top_p: '', min_p: '', repetition_penalty: '', _scene: '', _gen: {{id: 0, phase: 'idle', model: '', n_ctx: 0, \
         max_tokens: 0, emitted: 0, tok_s: 0, ttft_ms: 0, dt_ms: 0, prompt_tokens: 0, finish_reason: '', prefill_ms: 0, \
         kv_bytes_per_token: 0, decode_ms: 0, error: ''}}}}",
        chat_template(status.model("")).is_ok()
    )
}

/// Generation is running; the model cannot change meanwhile.
const BUSY: &str =
    "$_gen.phase == 'loading' || $_gen.phase == 'prefill' || $_gen.phase == 'decode'";

/// Whether `model` can take a chat turn, else why not.
pub(super) fn chat_template(model: Option<&ModelStatus>) -> Result<(), &'static str> {
    match model {
        Some(m) if m.backend.starts_with("federated/") => Err("not on federated routes"),
        Some(m) if m.chat_format.is_empty() => Err("no chat template"),
        Some(_) => Ok(()),
        None => Err("no such model"),
    }
}

pub(super) fn page(view: View, status: &Status, pin: &str) -> String {
    html!(
        lang = "en",
        head!(
            meta!(charset = "utf-8"),
            meta!(
                name = "viewport",
                content = "width=device-width,initial-scale=1"
            ),
            title!(format!("mdma · {}", view.label())),
            link!(rel = "stylesheet", href = "/assets/style.css"),
            script!(r#type = "module", src = "/assets/scene.js"),
            script!(r#type = "module", src = "/assets/datastar.js"),
        ),
        body!(
            "data-signals" = signals(status),
            "data-init" = format!(
                "@get('/sse/status?view={}', {{filterSignals: {{include: /^$/}}}})",
                view.key()
            ),
            header!(
                class = "topbar",
                a!(class = "brand", href = "/", "mdma"),
                nav!(View::ALL.map(|v| a!(href = v.path(), class = active(v == view), v.label()))),
                status_pill(status),
            ),
            main!(
                class = format!("view view-{}", view.key()),
                section!(
                    class = "stage",
                    div!(
                        id = "scene",
                        "data-view" = view.key(),
                        "data-ignore-morph" = "",
                        "data-on-signal-patch" = "window.mdmaScene?.patch(patch)",
                        "data-on-signal-patch-filter" = "{include: /^_(scene|gen)(\\.|$)/}",
                    ),
                ),
                aside!(
                    class = "panel",
                    h1!(view.label()),
                    PreEscaped(panel(view, status, pin))
                ),
            ),
        ),
    )
    .into_string()
}

fn panel(view: View, status: &Status, pin: &str) -> String {
    match view {
        View::Memory => models_panel(status).into_string(),
        View::Federation => remote_panel(status, pin, &[]).into_string(),
        View::Playground => playground(status).into_string(),
    }
}

pub(super) fn status_pill(status: &Status) -> impl IntoHtml + use<> {
    let ready = status.models.iter().filter(|m| m.ready).count();
    let text = match &status.error {
        Some(error) => format!("{} · {error}", status.state),
        None if status.connected => format!(
            "{} · {} · {ready}/{} models",
            status.state,
            status.backend,
            status.models.len()
        ),
        None => status.state.clone(),
    };
    let battery = status.on_battery.then(|| {
        format!(
            " · battery {}",
            status
                .battery_percent
                .map_or_else(|| "?".into(), |p| format!("{p}%"))
        )
    });
    span!(
        id = "status-pill",
        class = format!("pill state-{}", status.state),
        text,
        battery
    )
}

pub(super) fn models_panel(status: &Status) -> impl IntoHtml + use<> {
    let rows: Vec<_> = status
        .models
        .iter()
        .map(|m| {
            let default = if m.name == status.default_model {
                " · default"
            } else {
                ""
            };
            let detail = join([
                m.architecture.as_str(),
                facts(m).as_str(),
                m.class.as_str(),
                m.kind.as_str(),
                target(m).as_str(),
            ]);
            // The trained context, when MDMA_N_CTX lowers it.
            let trained =
                (m.trained_ctx > m.n_ctx).then(|| small!(format!("of {}", m.trained_ctx)));
            tr!(
                td!(m.name.clone(), small!(format!("{detail}{default}"))),
                td!(class = "num", m.n_ctx.to_string(), trained),
                td!(class = "num", weights(m)),
                td!(residency(m)),
            )
        })
        .collect();
    let total: u64 = status.models.iter().map(|m| m.weight_bytes).sum();
    let hint = if status.memory.is_some() {
        "Live memory by kind, and what each model costs."
    } else {
        "Waiting for the server's first snapshot."
    };
    div!(
        id = "models-panel",
        p!(class = "hint", hint),
        status.memory.as_ref().map(memory_summary),
        table!(
            thead!(tr!(
                th!("model"),
                th!(class = "num", "n_ctx"),
                th!(class = "num", "weights"),
                th!("state")
            )),
            tbody!(rows),
        ),
        p!(
            class = "hint",
            format!(
                "{} on disk across {} models.",
                bytes(total),
                status.models.len()
            )
        ),
    )
}

fn memory_summary(memory: &MemorySnapshot) -> impl IntoHtml + use<> {
    let budget = if memory.limit_bytes == 0 {
        format!("{} reserved · no limit set", bytes(memory.used_bytes))
    } else {
        format!(
            "{} of {} reserved",
            bytes(memory.used_bytes),
            bytes(memory.limit_bytes)
        )
    };
    let device = memory.device.as_ref().map(|d| {
        let free = d
            .available_bytes
            .map(|b| format!(" · {} free", bytes(b)))
            .unwrap_or_default();
        format!(
            "{}{}{free}",
            d.name,
            if d.unified_memory {
                " · unified memory"
            } else {
                ""
            }
        )
    });
    let buffers = |label: &'static str, usage: Option<MemoryUsage>| {
        usage.map(|u| {
            let used = if u.limit_bytes == 0 {
                bytes(u.used_bytes)
            } else {
                format!("{} of {}", bytes(u.used_bytes), bytes(u.limit_bytes))
            };
            (dt!(label), dd!(used))
        })
    };
    dl!(
        class = "stats memory",
        dt!("native budget"),
        dd!(budget),
        dt!(class = "swatch weights", "weights"),
        dd!(bytes(memory.weights_bytes)),
        dt!(class = "swatch kv", "KV cache"),
        dd!(bytes(memory.kv_bytes)),
        dt!(class = "swatch workspace", "workspace"),
        dd!(bytes(memory.workspace_bytes)),
        dt!("in flight"),
        dd!(format!("{} requests", memory.active_requests)),
        device.map(|d| (dt!("device"), dd!(d))),
        buffers("device buffers", memory.device_buffers),
        buffers("host buffers", memory.host_buffers),
    )
}

/// `held`: the models this gateway keeps loaded.
pub(super) fn remote_panel(status: &Status, pin: &str, held: &[String]) -> impl IntoHtml + use<> {
    // This server, then one entry per peer node, as in the 3D view.
    let mut nodes: Vec<(String, Vec<&ModelStatus>)> = Vec::new();
    for m in status.models.iter().filter(|m| is_remote(m)) {
        let node = target(m);
        match nodes.iter_mut().find(|(name, _)| *name == node) {
            Some((_, models)) => models.push(m),
            None => nodes.push((node, vec![m])),
        }
    }
    nodes.sort_by(|(a, am), (b, bm)| (os(am[0]), a).cmp(&(os(bm[0]), b)));
    let local: Vec<&ModelStatus> = status.models.iter().filter(|m| !is_remote(m)).collect();
    let this = li!(
        class = "self",
        strong!(
            class = os_class(status.host.as_ref()),
            status
                .host
                .as_ref()
                .map_or("this server", |h| h.name.as_str())
                .to_owned()
        ),
        small!(status.host.as_ref().map(host_line).unwrap_or_default()),
        small!(
            class = "resources",
            status.host.as_ref().map(resources).unwrap_or_default()
        ),
        span!(
            class = format!("pill state-{}", status.state),
            status.state.clone()
        ),
        model_list(&local, held),
    );
    let alone = nodes.is_empty();
    let peers: Vec<_> = nodes
        .into_iter()
        .map(|(node, models)| {
            let ready = models.iter().all(|m| m.ready);
            let backend = models[0].backend.trim_start_matches("federated/");
            let mut detail = models[0].host.as_ref().map(host_line).unwrap_or_default();
            if !models[0]
                .host
                .as_ref()
                .is_some_and(|h| h.device.eq_ignore_ascii_case(backend))
            {
                detail = join([detail.as_str(), backend]);
            }
            li!(
                strong!(class = os_class(models[0].host.as_ref()), node),
                small!(detail),
                small!(
                    class = "resources",
                    models[0].host.as_ref().map(resources).unwrap_or_default()
                ),
                span!(class = health(ready), availability(ready)),
                model_list(&models, held),
            )
        })
        .collect();
    let enrolled = match &status.federation {
        Some(federation) if federation.authorized && federation.enabled => Some(PreEscaped(topology(federation).into_string())),
        Some(federation) if !federation.authorized => Some(PreEscaped((
            p!(class = "hint", "Enrolled federation peers and trust states are shown to operators. To make this gateway one, set on the server:"),
            pre!(class = "pin", format!("MDMA_OPERATOR_SPKI={pin}")),
        ).into_string())),
        _ => None,
    };
    div!(
        id = "remote-panel",
        ul!(class = "peers", this, peers),
        lease_error(""),
        alone.then(|| p!(class = "hint", "No peers.")),
        legend(),
        enrolled,
    )
}

fn model_list(models: &[&ModelStatus], held: &[String]) -> impl IntoHtml + use<> {
    ul!(models
        .iter()
        .map(|m| li!(
            span!(
                class = format!("class {}", m.class),
                title = m.class.clone(),
                m.name.clone()
            ),
            Some(facts(m))
                .filter(|facts| !facts.is_empty())
                .map(|facts| small!(class = "facts", facts)),
            m.capabilities
                .iter()
                .map(|c| span!(class = format!("cap {c}"), c.clone()))
                .collect::<Vec<_>>(),
            " ",
            residency(m),
            lease_button(m, held.contains(&m.name)),
        ))
        .collect::<Vec<_>>())
}

/// "loaded · 2 leases", "on disk", "loading", "failed".
fn residency(m: &ModelStatus) -> impl IntoHtml + use<> {
    let (class, text) = match m.residency.as_str() {
        "failed" => ("bad", "failed"),
        _ if !m.ready => ("bad", "unavailable"),
        "resident" => ("ok", "loaded"),
        "loading" => ("busy", "loading"),
        _ => ("idle", "on disk"),
    };
    let leases = match m.leases {
        0 => String::new(),
        1 => " · 1 lease".into(),
        n => format!(" · {n} leases"),
    };
    span!(
        class = format!("residency {class}"),
        title = m.error.clone(),
        format!("{text}{leases}")
    )
}

/// Load keeps a model loaded under this gateway's lease; Release gives it up.
fn lease_button(m: &ModelStatus, held: bool) -> Option<impl IntoHtml + use<>> {
    // Names go into a script; servers only allow these characters.
    let plain = m
        .name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    let (action, label, class) = if held {
        ("release", "Release", "small ghost")
    } else {
        ("hold", "Load", "small")
    };
    (plain && (held || m.ready && m.residency != "failed")).then(|| {
        button!(
            r#type = "button",
            class = class,
            "data-on:click" = format!("@post('/models/{action}?model={}')", m.name),
            label,
        )
    })
}

pub(super) fn lease_error(message: &str) -> impl IntoHtml + use<> {
    p!(id = "lease-error", class = "error", message.to_owned())
}

fn legend() -> impl IntoHtml {
    div!(
        class = "legend",
        [
            ("freebsd", "FreeBSD"),
            ("linux", "Linux"),
            ("macos", "macOS")
        ]
        .map(|(os, name)| span!(class = format!("os {os}"), name)),
        ["generative", "decision", "embedding", "reranker"]
            .map(|c| span!(class = format!("class {c}"), c)),
        ["reasoning", "tools", "vision", "audio"].map(|c| span!(class = format!("cap {c}"), c)),
        [
            ("ok", "loaded"),
            ("idle", "on disk"),
            ("busy", "loading"),
            ("bad", "failed")
        ]
        .map(|(c, text)| span!(class = format!("residency {c}"), text)),
    )
}

fn topology(federation: &FederationTopology) -> impl IntoHtml + use<> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let peers: Vec<_> = federation
        .peers
        .iter()
        .map(|peer| {
            let catalog = peer.catalog.as_ref().map_or_else(
                || "no fresh catalog".to_owned(),
                |c| {
                    format!(
                        "{} models · {} · fresh {}s more",
                        c.models.len(),
                        c.server_state,
                        c.expires_at_ms.saturating_sub(now_ms) / 1000
                    )
                },
            );
            let grants = format!(
                "sees {} · runs {} · routes {}",
                peer.catalog_exports.len(),
                peer.execute_exports.len(),
                peer.route_imports.len()
            );
            tr!(
                td!(peer.peer_id.clone(), small!(peer.endpoint.clone())),
                td!(peer
                    .host
                    .as_ref()
                    .map(|h| (h.name.clone(), small!(host_line(h))))),
                td!(class = format!("trust {}", peer.state), peer.state.clone()),
                td!(catalog, small!(grants)),
            )
        })
        .collect();
    let routes: Vec<_> = federation
        .routes
        .iter()
        .map(|r| {
            li!(
                strong!(r.alias.clone()),
                " → ",
                code!(format!("{}/{}", r.peer_id, r.model)),
                small!(format!(" {}", r.operation)),
            )
        })
        .collect();
    (
        h2!("Enrolled peers"),
        p!(
            class = "hint",
            format!(
                "{} · trust generation {}",
                federation.server_id, federation.trust_generation
            )
        ),
        table!(
            thead!(tr!(th!("peer"), th!("host"), th!("trust"), th!("catalog"))),
            tbody!(peers)
        ),
        federation
            .peers
            .is_empty()
            .then(|| p!("No peers are enrolled.")),
        (!routes.is_empty()).then(|| (h2!("Routes"), ul!(routes))),
    )
}

pub(super) fn model_select(status: &Status) -> impl IntoHtml + use<> {
    let options: Vec<_> = status
        .models
        .iter()
        .filter(|m| m.kind != "laya")
        .map(|m| {
            option!(
                value = m.name.clone(),
                format!(
                    "{} · {} · n_ctx {}{}{}",
                    m.name,
                    m.architecture,
                    m.n_ctx,
                    if m.chat_format.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", m.chat_format)
                    },
                    if m.ready { "" } else { " (unavailable)" },
                ),
            )
        })
        .collect();
    select!(
        id = "model-select",
        "data-bind:model" = "",
        "data-on:change" = "@post('/playground/model')",
        "data-attr:disabled" = BUSY,
        option!(value = "", format!("default ({})", status.default_model)),
        options,
    )
}

/// The chat toggle, off and disabled for a model that cannot chat.
pub(super) fn chat_toggle(model: Option<&ModelStatus>) -> impl IntoHtml + use<> {
    let unavailable = chat_template(model).err();
    let input = if unavailable.is_some() {
        input!(r#type = "checkbox", "data-bind:chat" = "", disabled = "").into_string()
    } else {
        input!(r#type = "checkbox", "data-bind:chat" = "").into_string()
    };
    label!(
        id = "chat-toggle",
        class = "check",
        PreEscaped(input),
        span!("chat template"),
        unavailable.map(|why| small!(class = "hint", format!(" ({why})"))),
    )
}

fn playground(status: &Status) -> impl IntoHtml + use<> {
    (
        label!(span!("model"), model_select(status)),
        label!(
            span!("prompt"),
            textarea!(
                rows = "6",
                "data-bind:prompt" = "",
                placeholder = "Write a prompt…"
            )
        ),
        chat_toggle(status.model("")),
        label!(
            "data-show" = "$chat",
            span!("system"),
            textarea!(
                rows = "2",
                "data-bind:system" = "",
                placeholder = "Optional instructions for the model"
            ),
        ),
        div!(
            class = "grid",
            label!(
                span!("max tokens"),
                input!(
                    r#type = "number",
                    min = "1",
                    max = "8192",
                    "data-bind:max_tokens" = ""
                )
            ),
            label!(
                span!("temperature"),
                input!(
                    r#type = "number",
                    step = "any",
                    min = "0",
                    placeholder = "0.8",
                    "data-bind:temperature" = ""
                )
            ),
            label!(
                span!("top p"),
                input!(
                    r#type = "number",
                    step = "any",
                    min = "0",
                    max = "1",
                    placeholder = "0.95",
                    "data-bind:top_p" = ""
                )
            ),
            label!(
                span!("min p"),
                input!(
                    r#type = "number",
                    step = "any",
                    min = "0",
                    max = "1",
                    placeholder = "0.05",
                    "data-bind:min_p" = ""
                )
            ),
            label!(
                span!("repetition penalty"),
                input!(
                    r#type = "number",
                    step = "any",
                    min = "0",
                    placeholder = "1.1",
                    "data-bind:repetition_penalty" = ""
                )
            ),
        ),
        div!(
            class = "actions",
            button!(
                r#type = "button",
                "data-on:click" = "@post('/playground/run')",
                "data-attr:disabled" = BUSY,
                "Run"
            ),
            button!(
                r#type = "button",
                class = "ghost",
                "data-show" = BUSY,
                "data-on:click" = "@post('/playground/stop?id=' + $_gen.id)",
                "Stop"
            ),
        ),
        p!(id = "run-error", class = "error"),
        dl!(
            class = "stats",
            dt!("phase"),
            dd!("data-text" =
                "$_gen.phase + ($_gen.finish_reason ? ' · ' + $_gen.finish_reason : '')"),
            dt!("prompt"),
            dd!("data-text" = "$_gen.prompt_tokens + ' tokens of ' + $_gen.n_ctx"),
            dt!("generated"),
            dd!("data-text" = "$_gen.emitted + ' / ' + $_gen.max_tokens"),
            dt!("decode"),
            dd!("data-text" = "$_gen.tok_s.toFixed(1) + ' tok/s'"),
            dt!("first token"),
            dd!(
                "data-text" = "$_gen.ttft_ms + ' ms' + ($_gen.prefill_ms ? ' (server ' + $_gen.prefill_ms + ' ms)' : '')"
            ),
        ),
        pre!(id = "output"),
    )
}

pub(super) fn run_error(message: &str) -> impl IntoHtml + use<> {
    p!(id = "run-error", class = "error", message.to_owned())
}

pub(super) fn output() -> impl IntoHtml {
    pre!(id = "output")
}

const fn active(current: bool) -> &'static str {
    if current { "active" } else { "" }
}

const fn availability(ready: bool) -> &'static str {
    if ready { "ready" } else { "unavailable" }
}

const fn health(ready: bool) -> &'static str {
    if ready { "ok" } else { "bad" }
}

fn weights(m: &ModelStatus) -> String {
    if m.weight_bytes == 0 {
        "—".to_owned()
    } else {
        bytes(m.weight_bytes)
    }
}

/// "1.1B · Q4_K_M", from the weights' header.
fn facts(m: &ModelStatus) -> String {
    join([
        parameters_label(m.parameters).as_str(),
        m.quantization.as_str(),
    ])
}

fn is_remote(m: &ModelStatus) -> bool {
    m.backend.starts_with("federated/")
}

/// Where a model runs: its node for remote models, else the local backend.
fn target(m: &ModelStatus) -> String {
    m.host
        .as_ref()
        .filter(|host| !host.name.is_empty())
        .map_or_else(|| m.backend.clone(), |host| host.name.clone())
}

fn os(m: &ModelStatus) -> &str {
    m.host.as_ref().map_or("", |h| h.os.as_str())
}

fn os_class(host: Option<&Host>) -> String {
    format!("os {}", host.map_or("", |h| h.os.as_str()))
}

/// "FreeBSD 16.0-CURRENT · amd64 · CPU"
fn host_line(host: &Host) -> String {
    join([
        host.os_name.as_str(),
        host.arch.as_str(),
        host.device.as_str(),
    ])
}

/// "RAM 41/62 GiB · GPU 4.1/8.0 GiB · disk 1.2/1.8 TiB free"
fn resources(host: &Host) -> String {
    let free = [
        ("RAM", host.memory_available_bytes, host.memory_bytes),
        ("GPU", host.device_available_bytes, host.device_memory_bytes),
        ("disk", host.disk_available_bytes, host.disk_bytes),
    ]
    .into_iter()
    .filter(|(_, _, total)| *total > 0)
    .map(|(label, free, total)| format!("{label} {}", share(free, total)))
    .collect::<Vec<_>>();
    if free.is_empty() {
        String::new()
    } else {
        format!("{} free", free.join(" · "))
    }
}

/// `free/total` in the unit of `total`.
fn share(free: u64, total: u64) -> String {
    let (unit, scale) = if total >= 1 << 40 {
        ("TiB", (1_u64 << 40) as f64)
    } else {
        ("GiB", (1_u64 << 30) as f64)
    };
    let number = |bytes: u64| {
        let value = bytes as f64 / scale;
        if value < 10.0 {
            format!("{value:.1}")
        } else {
            format!("{value:.0}")
        }
    };
    format!("{}/{} {unit}", number(free), number(total))
}

fn join<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    parts
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

pub(super) fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
#[path = "pages/tests.rs"]
mod tests;
