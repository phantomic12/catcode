use crate::*;

pub(crate) async fn run() {
    // One-time rename migration: move the pre-rename on-disk layout
    // (~/.config/umans-harness/, ~/.umans-harness/) to the current names
    // (~/.config/catalyst-code/, ~/.catalyst-code/), preserving sessions,
    // memory, OAuth tokens, settings, and staged agent/skill/plugin files.
    // Runs before staging + config load so this run sees the migrated data.
    staging::migrate_legacy_dirs();
    // Stage the harness's global defaults (agents, orchestrator skill,
    // vision-handoff plugin) into ~/.catalyst-code/ on first run — shared
    // across every project, editable once, never per-project by default. Done
    // before config/plugin loading so staged files are picked up this run.
    let stage = staging::stage_if_needed();
    if stage.first_run {
        eprintln!(
            "[staging] first run: staged {} default file(s) into {}",
            stage.written.len(),
            stage.home.display()
        );
    }
    let mut cfg = config::load();
    // Explicit auth only — do not scan env vars or third-party OAuth stores.
    // Users must `/login` with an API key or complete this app's OAuth flow.
    let _ = config::auto_login_env_presets(&mut cfg);
    // Install the process-global sandbox execution backend from the loaded
    // config (host when sandbox=none, Microsandbox microVM when enabled). The
    // microVM itself boots lazily on the first agent-controlled exec.
    crate::sandbox::init_from_config(std::sync::Arc::new(cfg.clone()));
    // connect_timeout only: no total .timeout() — long reasoning streams must
    // not be aborted mid-body. Per-chunk idle timeouts live in stream_turn.
    // system-proxy / http2 / native roots come from Cargo features (see
    // core/Cargo.toml); Client::builder() picks them up automatically.
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .tcp_nodelay(true)
        .build()
        .expect("client");

    // Load plugins before initial model discovery. Plugin OAuth credentials are
    // durable, and their token action may also start a provider sidecar (Cursor
    // does this). Discovering with `None` here meant the saved provider was
    // restored as active but omitted from the first model snapshot until the
    // user ran /login again or manually switched providers.
    let plugin_manager = PluginManager::new_with_global_plugins(
        cfg.plugin_dir.clone(),
        cfg.workspace.clone(),
        cfg.trust_project_plugins,
    );
    for name in &cfg.plugins_disabled {
        let _ = plugin_manager.disable(name);
    }

    // Discover models up front. In the multi-login model, models are aggregated
    // across all logged-in providers (configured + key available) so `/models`
    // can mix providers. At init there are no runtime keys yet beyond the
    // persisted ones already in cfg, so this resolves from config/env.
    let init_provider = cfg.resolve_provider(&HashMap::new());
    let init_keys = cfg.persisted_keys.clone();
    let models = aggregate_models_for(
        &cfg,
        &init_keys,
        cfg.active_provider.as_deref(),
        &client,
        Some(&plugin_manager),
        false,
    )
    .await;
    // Install the process-wide logger first so provider HTTP / protocol emit can
    // write verbose records without a State handle. State keeps its own Logger
    // handle on the same path (append-mode; both FDs are fine).
    crate::logging::install_global_logger(std::sync::Arc::new(Logger::with_verbose(
        cfg.debug_log.as_deref(),
        cfg.debug_verbose,
    )));
    let logger = Logger::with_verbose(cfg.debug_log.as_deref(), cfg.debug_verbose);
    logger.log(
        "init",
        json!({
            "workspace": cfg.workspace.display().to_string(),
            "provider": init_provider.name,
            "kind": init_provider.kind.as_str(),
            "base_url": init_provider.base_url,
            "approval": cfg.approval.as_str(),
            "debug_verbose": cfg.debug_verbose,
        }),
    );

    // Resume session if configured and present. A future-version session file
    // returns Err (surfaced to the user via an `error` event at Init) rather
    // than silently starting blank.
    let (resumed, session_error, session_recovery_warnings, unfinished_runs) =
        match cfg.session_file.as_ref() {
            Some(p) => match session::load_report(p.as_path()) {
                Ok(report) => (
                    report.messages,
                    None,
                    report.warnings,
                    report.unfinished_runs,
                ),
                Err(e) => (Vec::new(), Some(e), Vec::new(), Vec::new()),
            },
            None => (Vec::new(), None, Vec::new(), Vec::new()),
        };
    // Persisted cumulative stats travel with the session file (sidecar
    // <session>.stats) so `/stats` survives a restart — previously in-memory
    // only, so reopening showed zeros for tokens/turns.
    let init_stats: session::SessionStats = cfg
        .session_file
        .as_ref()
        .map(|p| session::load_stats(p.as_path()))
        .unwrap_or_default();
    logger.set_turns(init_stats.turns);
    // Persisted "always" approval escalations travel with the session file
    // (sidecar <session>.escalations) so a restart doesn't un-gate kinds the
    // user already approved.
    let init_escalations: HashSet<&'static str> = cfg
        .session_file
        .as_ref()
        .map(|p| session::load_escalations(p.as_path()))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|s| match s.as_str() {
            "destructive" => Some("destructive"),
            "readonly" => Some("readonly"),
            _ => None,
        })
        .collect();
    // Ensure the session file exists (header only) so the active session is
    // always listed by `list_sessions`, even before the first message lands.
    if let Some(p) = cfg.session_file.as_ref() {
        session::ensure(p.as_path());
        for run in &unfinished_runs {
            session::append_activity_state(
                p,
                &run.session_id,
                &run.run_id,
                run.kind.as_deref().unwrap_or("run"),
                run.parent_run_id.as_deref(),
                run.tool_call_id.as_deref(),
                session::RunState::Interrupted,
                Some("core restarted before the run reached a terminal state"),
            );
        }
    }

    // Pre-compute token estimate for resumed conversation.
    let init_est = estimate_messages_tokens(&resumed);

    let vision_cfg = VisionConfig::load(&cfg.workspace);
    let state = Arc::new(State {
        cfg: RwLock::new(cfg),
        client: client.clone(),
        api_keys: RwLock::new(HashMap::new()),
        active_provider: RwLock::new(None),
        conversation: Mutex::new(resumed),
        models: RwLock::new(models),
        runtime: Arc::new(RuntimeCoordinator::new()),
        current: Mutex::new(None),
        handle: Mutex::new(None),
        pending: Mutex::new(std::collections::HashMap::new()),
        pending_asks: Mutex::new(std::collections::HashMap::new()),
        pending_sudos: Mutex::new(std::collections::HashMap::new()),
        logger,
        tokens_in: Mutex::new(init_stats.tokens_in),
        tokens_out: Mutex::new(init_stats.tokens_out),
        cached_tokens: Mutex::new(init_stats.cached_tokens),
        escalated_kinds: Mutex::new(init_escalations),
        queued: Mutex::new(None),
        pending_bash: Mutex::new(Vec::new()),
        plugin_manager,
        vision: RwLock::new(vision_cfg),
        last_turn_time: Mutex::new(std::time::Instant::now()),
        estimated_tokens: Mutex::new(init_est),
        last_real_prompt_tokens: Mutex::new(None),
        conv_len_at_last_real: Mutex::new(0),
        last_model: Mutex::new(None),
        last_turn_metrics: Mutex::new(None),

        work_state: Mutex::new(WorkState::default()),
        open_advisories: Mutex::new(Vec::new()),
        goal: Mutex::new(goal::GoalMode::default()),
        goal_deploy_cancel: Mutex::new(None),
        goal_wrapup_active: std::sync::atomic::AtomicBool::new(false),
        intercom: IntercomBus::new(),
        subagent_runs: Mutex::new(std::collections::HashMap::new()),
        pending_oauth: Mutex::new(None),
        peers: Mutex::new(Vec::new()),
        last_concurrency_note: Mutex::new(None),
        tool_output_cache: Mutex::new(tool_cache::ToolOutputCache::new()),
        enabled_deferred_tools: Mutex::new(std::collections::HashSet::new()),
        eval_history: Mutex::new(std::collections::HashMap::new()),
        undo_count: std::sync::atomic::AtomicU64::new(0),
        auto_checkpoint_taken: std::sync::atomic::AtomicBool::new(false),
        skill_read_count: std::sync::atomic::AtomicU64::new(0),
        compaction_count: std::sync::atomic::AtomicU64::new(init_stats.compactions),
    });
    if let Some(path) = state.cfg.read().await.session_file.clone() {
        state.intercom.configure_journal(&path);
        state.intercom.replay_undelivered();
    }
    protocol::install_runtime(&state.runtime);

    // Seed runtime API keys from the TUI-persisted `provider_keys`/`api_key`
    // (read from settings.json by Config::load). A key set via `/login` or the
    // settings modal is saved by the TUI into settings.json; loading it here
    // makes it survive a restart and take precedence over provider config/env
    // keys (runtime keys are checked first in provider resolution).
    {
        let cfg = state.cfg.read().await;
        for (name, key) in cfg.persisted_keys.iter() {
            // Diagnostic for silent shadowing: settings.json `provider_keys.<name>`
            // (a runtime key) wins over the same provider's explicit `api_key` in
            // config.json at request time. When the two diverge — e.g. config.json
            // was edited to a fresh key but settings.json still holds a stale /
            // pasted value from an earlier login — catcode fails with opaque
            // `HTTP 401 Invalid API key` and there is otherwise no visible cause.
            // Log a warning so the shadowing is observable instead of invisible.
            if let Some(p) = cfg.providers.iter().find(|p| &p.name == name) {
                if let Some(cfg_key) = p.api_key.as_ref().filter(|k| !k.is_empty()) {
                    if cfg_key != key {
                        eprintln!(
                            "[catalyst-code] warning: persisted `provider_keys.{name}` in \
                             settings.json differs from the same provider's `api_key` in \
                             config.json and shadows it (the persisted key wins at request \
                             time). To use the config.json key run /logout {name} or remove \
                             `provider_keys.{name}` from settings.json."
                        );
                    }
                }
            }
            state
                .api_keys
                .write()
                .await
                .insert(name.clone(), key.clone());
        }
    }

    // Background poll of the Umans gateway's `/v1/usage` endpoint so the footer
    // can show a LIVE, account-wide concurrency usage (used/limit) ahead of tps.
    // Updated every few seconds, independent of turns. Polls ANY configured Umans
    // provider that has a key (not just the active one) so conc stays live even
    // when a non-Umans provider is active but a Umans model is selected. Emits
    // `umans_conc { used, limit, provider }` — `provider` is the Umans provider
    // name it polled, so the UI only renders the field when the SELECTED model
    // routes to that provider (a Gemini/OpenAI model selected → hidden). Both
    // null + no provider when no Umans provider is available, to clear the UI.
    {
        let st = state.clone();
        let cl = client.clone();
        tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(5);
            let mut last_provider: Option<String> = None;
            loop {
                match st.umans_provider_with_key().await {
                    Some(rp) => {
                        let name = rp.name.clone();
                        let (used, limit) = match rp.api_key.as_deref() {
                            Some(k) => {
                                match provider::fetch_umans_usage(&cl, &rp.base_url, k).await {
                                    Some(u) => (u.used, u.limit),
                                    None => (None, None),
                                }
                            }
                            None => (None, None),
                        };
                        let used_v = used.map(Value::from).unwrap_or(Value::Null);
                        let limit_v = limit.map(Value::from).unwrap_or(Value::Null);
                        emit(
                            &Event::new("umans_conc")
                                .with("used", used_v)
                                .with("limit", limit_v)
                                .with("provider", json!(name)),
                        );
                        last_provider = Some(name);
                    }
                    None => {
                        if last_provider.take().is_some() {
                            emit(
                                &Event::new("umans_conc")
                                    .with("used", Value::Null)
                                    .with("limit", Value::Null),
                            );
                        }
                    }
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    // Startup background refresh of the Umans model cache. The TTL-gated cache
    // (8h, see provider::MODELS_CACHE_TTL) means newly-added Umans models
    // wouldn't appear until the TTL expires; this one-shot task forces a live
    // `/models/info` fetch on every launch so new models are cached locally and
    // surface in `/models` without a restart. Non-blocking: init already loaded
    // the (possibly stale) cached models for an instant first render, and we
    // only re-emit a `models` event when the live id set actually changed, so
    // the TUI's model selection (by id) is preserved and an unchanged or
    // offline fetch causes no spurious churn. Mirrors the launch-time update
    // check + conc poll: background, best-effort, silent on failure.
    {
        let st = state.clone();
        let cl = client.clone();
        tokio::spawn(async move {
            let Some(rp) = st.umans_provider_for_model_refresh().await else {
                return; // No Umans provider (active or configured) — nothing to refresh.
            };
            // Snapshot the model ids we currently hold for this provider so we
            // can tell whether the live fetch actually changed anything.
            let prev_ids: std::collections::HashSet<String> = {
                let models = st.models.read().await;
                models
                    .iter()
                    .filter(|m| m.provider == rp.name)
                    .map(|m| m.id.clone())
                    .collect()
            };
            // Force a live fetch (bypassing the TTL) and rewrite the cache. On
            // HTTP failure this falls back to the stale cache / curated
            // snapshot, so the id set is unchanged and we skip the re-emit.
            let live = provider::discover_models_force_refresh(&cl, &rp).await;
            let new_ids: std::collections::HashSet<String> =
                live.iter().map(|m| m.id.clone()).collect();
            if new_ids == prev_ids {
                return; // No new/removed models — leave the in-memory list alone.
            }
            // The live set changed: re-aggregate (reads the now-updated cache
            // for every provider) and re-emit `models` so the TUI/web pick up
            // the new models mid-session without a restart.
            st.refresh_models(&cl).await;
        });
    }

    // Cross-session presence: publish this session's rolling work-state so other
    // processes in the SAME workspace can detect concurrent activity (and stop
    // "fixing" phantom errors caused by a neighbor's in-flight edits). Per-pid
    // JSON file under ~/.config/catalyst-code/presence/<hash(cwd)>/, rewritten
    // every few seconds; stale records reaped by readers. Awareness only — no
    // coordination/locking. The `workspace_activity` tool + the anomaly nudge
    // in `run_turn` consume this; the cached peer snapshot avoids a filesystem
    // read on every tool result.
    {
        let st = state.clone();
        let pid = std::process::id();
        let started = presence::unix_now();
        let presence_ws = {
            let cfg = state.cfg.read().await;
            cfg.workspace.clone()
        };
        // Publish immediately so a peer checking right after we start sees us.
        {
            let ws = st.work_state.lock().await;
            let session_id = st
                .cfg
                .read()
                .await
                .session_file
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(String::from);
            let model = st.last_model.lock().await.clone();
            let rec =
                presence::PresenceRecord::from_work_state(&ws, pid, session_id, model, started);
            drop(ws);
            presence::write_presence(&presence_ws, pid, &rec);
        }
        tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(8);
            loop {
                tokio::time::sleep(interval).await;
                let ws = st.work_state.lock().await;
                let session_id = st
                    .cfg
                    .read()
                    .await
                    .session_file
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(String::from);
                let model = st.last_model.lock().await.clone();
                let rec =
                    presence::PresenceRecord::from_work_state(&ws, pid, session_id, model, started);
                drop(ws);
                presence::write_presence(&presence_ws, pid, &rec);
                // Refresh the cached peer snapshot so the anomaly nudge stays
                // current without a filesystem read on the hot path.
                *st.peers.lock().await = presence::read_peers(&presence_ws, pid);
            }
        });
    }

    const MAX_COMMAND_BYTES: usize = 8 * 1024 * 1024;
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut command_bytes = Vec::new();
    let mut discarding_oversized = false;

    'commands: loop {
        let line = loop {
            let available = match reader.fill_buf().await {
                Ok(bytes) if bytes.is_empty() => {
                    if discarding_oversized || command_bytes.is_empty() {
                        break 'commands;
                    }
                    match String::from_utf8(std::mem::take(&mut command_bytes)) {
                        Ok(line) => break line,
                        Err(e) => {
                            emit(
                                &Event::new("error")
                                    .with("code", json!("invalid_command"))
                                    .with(
                                        "message",
                                        json!(format!("command is not valid UTF-8: {e}")),
                                    ),
                            );
                            break 'commands;
                        }
                    }
                }
                Ok(bytes) => bytes,
                Err(e) => {
                    emit(
                        &Event::new("error")
                            .with("code", json!("protocol_read_error"))
                            .with("message", json!(format!("stdin read failed: {e}"))),
                    );
                    break 'commands;
                }
            };
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |pos| pos + 1);

            if discarding_oversized {
                reader.consume(consumed);
                if newline.is_some() {
                    discarding_oversized = false;
                }
                continue;
            }
            if command_bytes.len().saturating_add(consumed) > MAX_COMMAND_BYTES {
                command_bytes.clear();
                discarding_oversized = newline.is_none();
                reader.consume(consumed);
                emit(
                    &Event::new("error")
                        .with("code", json!("invalid_command"))
                        .with(
                            "message",
                            json!(format!("command exceeds {MAX_COMMAND_BYTES} byte limit")),
                        ),
                );
                continue;
            }
            command_bytes.extend_from_slice(&available[..consumed]);
            reader.consume(consumed);
            if newline.is_none() {
                continue;
            }
            while command_bytes
                .last()
                .is_some_and(|b| matches!(b, b'\n' | b'\r'))
            {
                command_bytes.pop();
            }
            match String::from_utf8(std::mem::take(&mut command_bytes)) {
                Ok(line) => break line,
                Err(e) => {
                    emit(
                        &Event::new("error")
                            .with("code", json!("invalid_command"))
                            .with("message", json!(format!("command is not valid UTF-8: {e}"))),
                    );
                }
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        // Verbose mode: mirror every inbound protocol command (redacted) so a
        // `catcode --debug` session can reconstruct the full wire conversation.
        if crate::logging::debug_verbose() {
            match serde_json::from_str::<Value>(&line) {
                Ok(v) => crate::logging::global_log(
                    "command",
                    json!({ "raw": crate::logging::truncate_json_for_log(&v, crate::logging::VERBOSE_PAYLOAD_CAP) }),
                ),
                Err(_) => {
                    let (text, len, truncated) = crate::logging::truncate_for_log(
                        &line,
                        crate::logging::VERBOSE_PAYLOAD_CAP,
                    );
                    crate::logging::global_log(
                        "command",
                        json!({
                            "raw_text": text,
                            "original_len": len,
                            "truncated": truncated,
                            "parse_error": true,
                        }),
                    );
                }
            }
        }
        let cmd = match serde_json::from_str::<Command>(&line) {
            Ok(c) => c,
            Err(e) => {
                if crate::logging::debug_verbose() {
                    crate::logging::global_log(
                        "command_error",
                        json!({ "error": format!("bad command: {e}") }),
                    );
                }
                emit(
                    &Event::new("error")
                        .with("code", json!("invalid_command"))
                        .with("message", json!(format!("bad command: {e}"))),
                );
                continue;
            }
        };
        match cmd {
            Command::Init {
                protocol_version: client_protocol_version,
                client: client_info,
            } => {
                let models = state.models.read().await.clone();
                // Enrich OAuth so SuperGrok / Claude / Gemini look signed-in on
                // startup (they store tokens on disk, not as api_key in config).
                let rp = state.resolved_provider_enriched().await;
                // `authed` = the ACTIVE provider has a usable key (footer/status).
                // `any_logged_in` = ANY configured provider is logged in — the TUI
                // uses this to allow multi-provider sends (model routes to its
                // owner) even when the active/fallback provider is unauthenticated
                // (stale settings.active_provider, expired OAuth, etc.). Without
                // this, Linux/WSL users with a broken active provider and working
                // secondary keys saw "not authenticated" and never fired requests.
                let authed = rp.api_key.is_some();
                let cfg = state.cfg.read().await;
                // `any_logged_in` needs the cfg + runtime key map; compute before
                // we start using `cfg` for the ready payload so we don't hold
                // two overlapping read guards on the same lock.
                let any_logged_in = {
                    let keys = state.api_keys.read().await;
                    !logged_in_providers_for(&cfg, &keys, Some(&state.plugin_manager)).is_empty()
                        || authed
                };
                let conv_len = state.conversation.lock().await.len();
                let loaded_plugins: Vec<String> =
                    state.plugin_manager.list().keys().cloned().collect();
                // Only surface skips that left the plugin unavailable (a same-named
                // global copy may still be loaded — common for staged defaults)
                // AND have no recorded decision yet — a deliberately DENIED plugin
                // must not nag on every startup (the user already answered).
                let pending_trust = state.plugin_manager.pending_trust_plugins();
                let skipped_unavailable: Vec<String> = pending_trust
                    .iter()
                    .map(|p| p.name.clone())
                    .filter(|n| !loaded_plugins.iter().any(|l| l == n))
                    .collect();
                emit(
                    &Event::new("ready")
                        .with("models", json!(models))
                        .with("authed", json!(authed))
                        .with("anyLoggedIn", json!(any_logged_in))
                        .with("workspace", json!(cfg.workspace.display().to_string()))
                        .with("approval", json!(cfg.approval.as_str()))
                        .with("base_url", json!(rp.base_url))
                        .with("provider", json!(rp.name))
                        .with("providerKind", json!(rp.kind.as_str()))
                        .with("providers", json!(cfg.provider_names()))
                        .with(
                            "providerPresets",
                            json!(provider_presets_json(&cfg, Some(&state.plugin_manager))),
                        )
                        .with("bash_timeout_secs", json!(cfg.bash_timeout_secs))
                        .with("auto_compact", json!(cfg.auto_compact))
                        .with("context_compact_at", json!(cfg.context_compact_at))
                        .with("context_digest_at", json!(cfg.context_digest_at))
                        .with("sandbox", json!(cfg.sandbox.as_str()))
                        .with(
                            "shell",
                            json!(crate::sandbox::policy::effective_shell_kind().as_str()),
                        )
                        .with("sandboxImage", json!(cfg.sandbox_image))
                        .with("sandboxCpus", json!(cfg.sandbox_cpus))
                        .with("sandboxMemoryMb", json!(cfg.sandbox_memory_mb))
                        .with(
                            "sandboxNetworkMode",
                            json!(cfg.sandbox_network_mode.as_str()),
                        )
                        .with(
                            "sandboxReady",
                            json!(crate::sandbox::sandbox_status().await.ready),
                        )
                        .with("resumed_messages", json!(conv_len))
                        .with("plugins", json!(loaded_plugins))
                        .with("plugins_skipped", json!(skipped_unavailable.clone())),
                );
                emit(
                    &Event::new("protocol_hello")
                        .with("version", json!(env!("CARGO_PKG_VERSION")))
                        .with("protocol_version", json!(protocol::PROTOCOL_VERSION))
                        .with("min_client", json!("0.2.0"))
                        .with("client_protocol_version", json!(client_protocol_version))
                        .with(
                            "client",
                            json!(client_info.as_ref().map(|client| json!({
                                "name": client.name,
                                "version": client.version,
                                "capabilities": client.capabilities,
                            }))),
                        )
                        .with("capabilities", json!(protocol::CAPABILITIES)),
                );
                // Auto trust prompt: when project plugins are gated off with NO
                // recorded decision, surface the trust modal once (decisions
                // are persisted, so it does not re-appear on later loads).
                let pending_trust = state.plugin_manager.pending_trust_plugins();
                let plugins = pending_trust
                    .into_iter()
                    .filter(|p| !loaded_plugins.iter().any(|name| name == &p.name))
                    .map(|p| {
                        json!({
                            "name": p.name,
                            "version": p.version,
                            "description": p.description,
                            "path": p.path.display().to_string(),
                            "decision": p.decision.unwrap_or_default(),
                        })
                    })
                    .collect::<Vec<_>>();
                if !plugins.is_empty() {
                    emit(&Event::new("plugin_trust_prompt").with("plugins", json!(plugins)));
                }
                // Tell the user when the harness staged its global defaults
                // (first run) so the global ~/.catalyst-code/ layout is
                // discoverable.
                if stage.first_run {
                    emit(
                        &Event::new("info").with(
                            "message",
                            json!(format!(
                                "First run: staged {} default file(s) into {} — agents, the pi-subagents skill, and the vision-handoff plugin now live globally and are shared across all projects. Edit them there to customize; drop a file in a project's own .catalyst-code/ to override for that project only.",
                                stage.written.len(),
                                stage.home.display()
                            )),
                        ),
                    );
                }
                // Surface a future-version session-load error to the user.
                if let Some(e) = session_error.as_ref() {
                    emit(&Event::new("error").with("message", json!(e)));
                }
                if !session_recovery_warnings.is_empty() || !unfinished_runs.is_empty() {
                    emit(
                        &Event::new("session_recovered")
                            .with("warnings", json!(session_recovery_warnings))
                            .with(
                                "interrupted_runs",
                                json!(unfinished_runs
                                    .iter()
                                    .map(|run| &run.run_id)
                                    .collect::<Vec<_>>()),
                            )
                            .with(
                                "interrupted_activities",
                                json!(unfinished_runs
                                    .iter()
                                    .map(|run| json!({
                                        "run_id": run.run_id,
                                        "kind": run.kind.as_deref().unwrap_or("run"),
                                        "parent_run_id": run.parent_run_id,
                                        "tool_call_id": run.tool_call_id,
                                    }))
                                    .collect::<Vec<_>>()),
                            ),
                    );
                }
                // Publish the discoverable-skills list so the TUI/web can
                // populate their `/skill:<name>` autocomplete immediately.
                emit_skills_event(&cfg.workspace);
                // Same for subagents (builtin + user + project overlays).
                emit_agents_event(&cfg.workspace, &cfg);
                // Replay any resumed conversation so the TUI shows prior history
                // on launch instead of starting from an empty transcript.
                if conv_len > 0 {
                    let conv = state.conversation.lock().await;
                    let visible: Vec<Value> = conv
                        .iter()
                        .filter(|m| !m.is_system())
                        .map(Value::from)
                        .collect();
                    let est = estimate_messages_tokens(&conv);
                    emit(
                        &Event::new("history")
                            .with("messages", json!(visible))
                            .with("tokens_in", json!(est)),
                    );
                    // The conversation was left mid-`ask` by a prior core
                    // restart (assistant `ask` tool_call with no tool result).
                    // Tell the user a message will re-present the question so
                    // they don't see a wedged transcript with no explanation.
                    if find_trailing_unanswered_ask(&conv[..]).is_some() {
                        emit(&Event::new("info").with(
                            "message",
                            json!(
                                "A question from the previous session was interrupted by a restart. Send any message to answer it and continue."
                            ),
                        ));
                    }
                }
            }
            Command::SetKey { api_key, provider } => {
                // Apply the key to a named provider, or to the active provider
                // when no name is given (backward-compatible with the pre-provider
                // single-key flow, which lands in the "default" slot). Setting a
                // key "logs in" that provider, so re-aggregate models so its
                // models appear in `/models` alongside any others logged in.
                let name = match provider {
                    Some(p) => p,
                    None => state.resolved_provider().await.name,
                };
                state.api_keys.write().await.insert(name.clone(), api_key);
                state.logger.log("set_key", json!({ "provider": name }));
                emit(
                    &Event::new("authed")
                        .with("ok", json!(true))
                        .with("provider", json!(name)),
                );
                {
                    let st_rm = state.clone();
                    let cl_rm = client.clone();
                    tokio::spawn(async move {
                        st_rm.refresh_models(&cl_rm).await;
                    });
                }
            }
            Command::SetSearchKey { provider, api_key } => {
                // Set or clear a search-tool API key (Exa / Tavily) for
                // `web_search`. Persisted to config.json `search_keys` so it
                // survives restarts; `search_tool` reads it ahead of the
                // `EXA_API_KEY` / `TAVILY_API_KEY` env vars.
                let provider = provider.trim().to_ascii_lowercase();
                if provider != "exa" && provider != "tavily" {
                    emit(&Event::new("error").with(
                        "message",
                        json!(format!(
                            "set_search_key: unknown provider '{provider}' (expected 'exa' or 'tavily')"
                        )),
                    ));
                    continue;
                }
                let key = api_key.trim().to_string();
                let has_key = !key.is_empty();
                let snapshot = {
                    let mut cfg = state.cfg.write().await;
                    if has_key {
                        cfg.search_keys.insert(provider.clone(), key);
                    } else {
                        cfg.search_keys.remove(&provider);
                    }
                    cfg.search_keys.clone()
                };
                if let Err(e) = crate::config::save_search_keys(&snapshot) {
                    state.logger.log(
                        "set_search_key",
                        json!({ "provider": &provider, "err": e.to_string() }),
                    );
                    emit(&Event::new("error").with(
                        "message",
                        json!(format!("set_search_key: failed to persist: {e}")),
                    ));
                    continue;
                }
                state.logger.log(
                    "set_search_key",
                    json!({ "provider": &provider, "has_key": has_key }),
                );
                emit(
                    &Event::new("search_key_set")
                        .with("provider", json!(&provider))
                        .with("has_key", json!(has_key)),
                );
            }
            Command::SetProvider { name } => {
                // Set the default/fallback provider. In the multi-login model a
                // turn routes to the selected model's provider; this only matters
                // for model-less operations (compaction summarize) and legacy
                // models without a provider tag. Re-aggregate (don't wipe other
                // providers' models). Unknown names are ignored (stays put).
                {
                    let cfg = state.cfg.read().await;
                    if cfg.find_provider(&name).is_none() {
                        emit(&Event::new("error").with(
                            "message",
                            json!(format!("unknown provider '{name}'; not switching")),
                        ));
                        continue;
                    }
                }
                *state.active_provider.write().await = Some(name.clone());
                let rp = state.resolved_provider_enriched().await;
                state.logger.log(
                    "set_provider",
                    json!({ "provider": rp.name, "kind": rp.kind.as_str(), "base_url": rp.base_url }),
                );
                emit(
                    &Event::new("provider_changed")
                        .with("provider", json!(rp.name))
                        .with("kind", json!(rp.kind.as_str()))
                        .with("base_url", json!(rp.base_url))
                        .with("has_key", json!(rp.api_key.is_some())),
                );
                if rp.api_key.is_some() {
                    emit(
                        &Event::new("authed")
                            .with("ok", json!(true))
                            .with("provider", json!(rp.name)),
                    );
                }
                {
                    let st_rm = state.clone();
                    let cl_rm = client.clone();
                    tokio::spawn(async move {
                        st_rm.refresh_models(&cl_rm).await;
                    });
                }
            }
            Command::ListProviderPresets => {
                let cfg = state.cfg.read().await;
                emit(&Event::new("provider_presets").with(
                    "presets",
                    json!(provider_presets_json(&cfg, Some(&state.plugin_manager))),
                ));
            }
            Command::Login { preset, api_key } => {
                // Log in to a first-party provider from a preset: resolve the key
                // (explicit arg → preset env var), insert/replace into config,
                // seed the runtime key, persist, and re-aggregate models across
                // all logged-in providers so this provider's models join `/models`.
                // Multiple providers can be logged in at once. Most presets create
                // one config; OpenCode Go creates two (OpenAI-kind +
                // Anthropic-kind) sharing the base URL + key.
                let Some(p) = config::find_preset(&preset) else {
                    let available = config::PROVIDER_PRESETS
                        .iter()
                        .map(|p| p.id)
                        .collect::<Vec<_>>()
                        .join(", ");
                    emit(&Event::new("error").with(
                        "message",
                        json!(format!(
                            "unknown provider preset '{preset}'; available: {available}"
                        )),
                    ));
                    continue;
                };
                // API-key path: require an explicitly pasted key. Do not scan
                // the environment. Subscription OAuth is plugin-only
                // (`login_oauth`). Keyless login is only for local presets
                // with an empty api_key_env (Ollama / LM Studio).
                let key = api_key.filter(|s| !s.is_empty());
                if key.is_none() && !p.api_key_env.is_empty() {
                    emit(&Event::new("error").with(
                        "message",
                        json!(format!(
                            "no API key provided for '{}' — paste a key via /login (subscription OAuth is available via plugins)",
                            p.label
                        )),
                    ));
                    continue;
                }
                let configs = config::preset_provider_configs(p, key.clone());
                let name = configs[0].name.clone();
                // Insert or replace each provider config (e.g. opencode-go +
                // opencode-go-anthropic for the OpenCode Go preset).
                {
                    let mut cfg = state.cfg.write().await;
                    for pc in &configs {
                        if let Some(i) = cfg.providers.iter().position(|x| x.name == pc.name) {
                            cfg.providers[i] = pc.clone();
                        } else {
                            cfg.providers.push(pc.clone());
                        }
                    }
                }
                // Seed the runtime key for every config so the immediate turn
                // works without a restart (only when a key was actually resolved).
                if let Some(k) = &key {
                    let mut keys = state.api_keys.write().await;
                    for pc in &configs {
                        keys.insert(pc.name.clone(), k.clone());
                    }
                }
                // Make the newly logged-in provider the default/fallback (used
                // for model-less compaction and legacy models). This does NOT
                // restrict routing — the selected model still routes to its own
                // provider; it only picks the fallback.
                *state.active_provider.write().await = Some(name.clone());
                // Persist to the core-owned config.json (best-effort).
                {
                    let cfg = state.cfg.read().await;
                    if let Err(e) = config::save_providers_config(&cfg.providers, Some(&name)) {
                        emit(&Event::new("info").with(
                            "message",
                            json!(format!(
                                "logged into '{}' for this session (could not persist to config.json: {e})",
                                p.label
                            )),
                        ));
                    }
                }
                let rp = state.resolved_provider().await;
                state.logger.log(
                    "login",
                    json!({ "provider": name, "kind": p.kind.as_str(), "base_url": p.base_url, "has_key": key.is_some() }),
                );
                emit(&Event::new("info").with(
                    "message",
                    json!(if key.is_some() {
                        format!("logged into {}.", p.label)
                    } else {
                        format!("logged into {} (no API key required).", p.label)
                    }),
                ));
                emit(
                    &Event::new("provider_changed")
                        .with("provider", json!(rp.name))
                        .with("kind", json!(rp.kind.as_str()))
                        .with("base_url", json!(rp.base_url))
                        .with("has_key", json!(rp.api_key.is_some())),
                );
                // Keyless local presets (Ollama/LM Studio) are usable without a
                // key — report ok:true so clients do not block send (CORE_REVIEW).
                emit(
                    &Event::new("authed")
                        .with("ok", json!(true))
                        .with("provider", json!(name))
                        .with("has_key", json!(key.is_some())),
                );
                {
                    let st_rm = state.clone();
                    let cl_rm = client.clone();
                    tokio::spawn(async move {
                        st_rm.refresh_models(&cl_rm).await;
                    });
                }
            }
            Command::Logout { provider } => {
                // Log out of a provider: drop its runtime key, remove it from the
                // configured providers, persist, and re-aggregate models so its
                // models disappear from `/models`. The persisted TUI key (in
                // settings.json) is cleared by the TUI side.
                //
                // OpenCode Go is one subscription backed by two provider configs
                // (opencode-go + opencode-go-anthropic); logging out either drops
                // both so the user doesn't strand a half-configured subscription.
                let to_remove: Vec<String> =
                    if provider == "opencode-go" || provider == "opencode-go-anthropic" {
                        vec![
                            "opencode-go".to_string(),
                            "opencode-go-anthropic".to_string(),
                        ]
                    } else {
                        vec![provider.clone()]
                    };
                let existed;
                {
                    let mut cfg = state.cfg.write().await;
                    let before = cfg.providers.len();
                    cfg.providers.retain(|p| !to_remove.contains(&p.name));
                    existed = cfg.providers.len() != before;
                }
                for n in &to_remove {
                    state.api_keys.write().await.remove(n);
                }
                if !existed && provider != "default" {
                    emit(
                        &Event::new("error")
                            .with("message", json!(format!("not logged into '{provider}'"))),
                    );
                    continue;
                }
                // Delete plugin OAuth credential files so the provider is fully
                // logged out (not just its config/runtime key).
                for n in &to_remove {
                    state.plugin_manager.clear_oauth(n).await;
                }
                // If the active provider was one of those logged out, clear the
                // override so the fallback resolves to the first remaining / legacy.
                {
                    let active = state.active_provider.read().await.clone();
                    if active
                        .as_deref()
                        .map(|a| to_remove.iter().any(|n| n == a))
                        .unwrap_or(false)
                    {
                        *state.active_provider.write().await = None;
                    }
                }
                // Persist the trimmed provider list (fall back to the first
                // remaining provider, else legacy).
                {
                    let cfg = state.cfg.read().await;
                    let active = cfg.providers.first().map(|p| p.name.clone());
                    let _ = config::save_providers_config(&cfg.providers, active.as_deref());
                }
                state.logger.log("logout", json!({ "provider": provider }));
                emit(
                    &Event::new("info")
                        .with("message", json!(format!("logged out of '{}'", provider))),
                );
                let rp = state.resolved_provider_enriched().await;
                emit(
                    &Event::new("provider_changed")
                        .with("provider", json!(rp.name))
                        .with("kind", json!(rp.kind.as_str()))
                        .with("base_url", json!(rp.base_url))
                        .with("has_key", json!(rp.api_key.is_some())),
                );
                if rp.api_key.is_some() {
                    emit(
                        &Event::new("authed")
                            .with("ok", json!(true))
                            .with("provider", json!(rp.name)),
                    );
                } else {
                    emit(
                        &Event::new("authed")
                            .with("ok", json!(false))
                            .with("provider", json!(rp.name)),
                    );
                }
                {
                    let st_rm = state.clone();
                    let cl_rm = client.clone();
                    tokio::spawn(async move {
                        st_rm.refresh_models(&cl_rm).await;
                    });
                }
            }
            Command::AddCustomProvider {
                name,
                base_url,
                kind,
                api_key,
                api_key_env,
                headers,
                context_window,
                models_override,
            } => {
                // Add or update a custom provider with full config parity (same
                // fields hand-editing config.json supports). Validates, inserts/
                // replaces in cfg.providers, makes it the active/fallback,
                // persists, and re-aggregates models so its models join /models.
                let name = name.trim().to_string();
                let base_url = base_url.trim().to_string();
                if name.is_empty() || base_url.is_empty() {
                    emit(&Event::new("error").with(
                        "message",
                        json!("add_custom_provider: name and base_url are required"),
                    ));
                    continue;
                }
                let kind = kind
                    .as_deref()
                    .map(config::ProviderKind::parse)
                    .unwrap_or_default();
                let api_key = api_key
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let api_key_env = api_key_env
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let headers = config::parse_headers(headers.as_ref());
                let models_override = config::parse_models_override(models_override.as_ref());
                let pc = config::ProviderConfig {
                    name: name.clone(),
                    kind,
                    base_url,
                    api_key: api_key.clone(),
                    api_key_env,
                    headers,
                    context_window,
                    models_override,
                    models_endpoint: None,
                };
                let existed;
                {
                    let mut cfg = state.cfg.write().await;
                    if let Some(i) = cfg.providers.iter().position(|x| x.name == name) {
                        cfg.providers[i] = pc.clone();
                        existed = true;
                    } else {
                        cfg.providers.push(pc.clone());
                        existed = false;
                    }
                }
                // Seed the runtime key so the immediate turn works without a
                // restart. An env-var-only provider resolves its key at request
                // time; clearing a previously-seeded key on update (no literal
                // key this time) is handled by `resolved_provider` falling back
                // to config, so only write/remove the runtime slot explicitly.
                {
                    let mut keys = state.api_keys.write().await;
                    match &api_key {
                        Some(k) => {
                            keys.insert(name.clone(), k.clone());
                        }
                        None => {
                            keys.remove(&name);
                        }
                    }
                }
                *state.active_provider.write().await = Some(name.clone());
                {
                    let cfg = state.cfg.read().await;
                    if let Err(e) = config::save_providers_config(&cfg.providers, Some(&name)) {
                        emit(&Event::new("info").with(
                            "message",
                            json!(format!(
                                "provider '{name}' added for this session (could not persist to config.json: {e})"
                            )),
                        ));
                    }
                }
                state.logger.log(
                    "add_custom_provider",
                    json!({ "provider": pc.name, "kind": pc.kind.as_str(), "base_url": pc.base_url, "has_key": api_key.is_some(), "updated": existed }),
                );
                emit(&Event::new("info").with(
                    "message",
                    json!(if existed {
                        format!("updated provider '{name}'.")
                    } else {
                        format!("added provider '{name}'.")
                    }),
                ));
                let rp = state.resolved_provider_enriched().await;
                emit(
                    &Event::new("provider_changed")
                        .with("provider", json!(rp.name))
                        .with("kind", json!(rp.kind.as_str()))
                        .with("base_url", json!(rp.base_url))
                        .with("has_key", json!(rp.api_key.is_some())),
                );
                if rp.api_key.is_some() {
                    emit(
                        &Event::new("authed")
                            .with("ok", json!(true))
                            .with("provider", json!(rp.name)),
                    );
                }
                {
                    let st_rm = state.clone();
                    let cl_rm = client.clone();
                    tokio::spawn(async move {
                        st_rm.refresh_models(&cl_rm).await;
                    });
                }
            }
            Command::DiscoverProviderModels {
                base_url,
                kind,
                api_key,
                headers,
            } => {
                // Discover models from an endpoint WITHOUT persisting a
                // provider: build a throwaway ResolvedProvider, run the normal
                // discovery + models.dev enrichment, and emit a preview event.
                // Used by the add-custom-provider flow so the user can see (and
                // refine caps for) the models an endpoint exposes before
                // committing. The result is NOT added to /models — only a preview.
                //
                // IMPORTANT: run off the command loop. Live /models + models.dev
                // can take many seconds on a dead/slow host; awaiting here used
                // to freeze every other command (and the UI spinner never
                // cleared if the client only watched non-empty previews).
                let base_url = base_url.trim().to_string();
                if base_url.is_empty() {
                    emit(
                        &Event::new("provider_models_preview")
                            .with("models", json!([]))
                            .with("base_url", json!(""))
                            .with(
                                "error",
                                json!("discover_provider_models: base_url is required"),
                            ),
                    );
                    continue;
                }
                let kind = kind
                    .as_deref()
                    .map(config::ProviderKind::parse)
                    .unwrap_or_default();
                let api_key = api_key
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let headers = config::parse_headers(headers.as_ref());
                let rp = config::ResolvedProvider {
                    name: "__preview__".to_string(),
                    kind,
                    base_url: base_url.clone(),
                    api_key,
                    headers,
                    oauth: false,
                    context_window: None,
                    models_override: Vec::new(),
                    models_endpoint: None,
                };
                emit(&Event::new("info").with(
                    "message",
                    json!(format!("discovering models from {base_url}…")),
                ));
                let cl = client.clone();
                // Logger is not Clone (Mutex file handle). Log start here; the
                // spawn only emits the preview event so the command loop stays free.
                state.logger.log(
                    "discover_provider_models",
                    json!({ "base_url": base_url, "phase": "start" }),
                );
                tokio::spawn(async move {
                    // Force a live fetch so a stale disk cache from a previous
                    // attempt at this URL doesn't short-circuit the preview.
                    let models = provider::discover_models_force_refresh(&cl, &rp).await;
                    let error = if models.is_empty() {
                        Some(format!(
                            "no models discovered from {base_url} — check base URL, API key, wire kind, and that the endpoint serves /models"
                        ))
                    } else {
                        None
                    };
                    let mut ev = Event::new("provider_models_preview")
                        .with("models", json!(models))
                        .with("base_url", json!(base_url))
                        .with("count", json!(models.len()));
                    if let Some(err) = error {
                        ev = ev.with("error", json!(err));
                    }
                    emit(&ev);
                });
            }
            Command::LoginOauth { preset } => {
                // Plugin-declared subscription OAuth only. Built-in vendor
                // OAuth was removed from core — install a plugin that declares
                // an `oauth` block (e.g. catcode-chatgpt-provider).
                let plugin_login = state.plugin_manager.supports_oauth_login(&preset);
                if !plugin_login {
                    emit(&Event::new("error").with(
                        "message",
                        json!(format!(
                            "'{preset}' has no plugin OAuth login — install a plugin that declares oauth.provider_id=\"{preset}\", or paste an API key via /login"
                        )),
                    ));
                    continue;
                }
                let label = state
                    .plugin_manager
                    .oauth_config(&preset)
                    .map(|c| c.label)
                    .unwrap_or_else(|| preset.clone());
                emit(&Event::new("info").with(
                    "message",
                    json!(format!("starting OAuth login for {label}…")),
                ));
                let st = state.clone();
                let cl = client.clone();
                // Generation counter: a second /login cancels applying an older
                // in-flight OAuth (user re-ran login with a new device code).
                static OAUTH_GEN: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let gen = OAUTH_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                let session = state.runtime.session_context();
                let Some(resource) = state.runtime.register_session_resource(
                    &session,
                    ResourceKind::Task,
                    format!("oauth_login:{preset}"),
                ) else {
                    emit(&Event::new("error").with(
                        "message",
                        json!("OAuth login was not started because its session is stale"),
                    ));
                    continue;
                };
                let oauth_cancel = resource.cancellation().clone();
                tokio::spawn(runtime::scope_session(session.clone(), async move {
                    // Use free protocol::emit (Send) — not a captured &dyn Fn.
                    let prompt_emit = |p: oauth::OAuthPrompt| {
                        emit(
                            &Event::new("oauth_prompt")
                                .with("url", json!(p.url))
                                .with("code", json!(p.code))
                                .with("message", json!(p.message)),
                        );
                    };
                    let outcome = st.plugin_manager.oauth_login(&preset, &prompt_emit).await;
                    if oauth_cancel.is_cancelled() || !st.runtime.is_session_active(&session) {
                        return;
                    }
                    // Stale attempt (user started another login) — drop result.
                    if OAUTH_GEN.load(std::sync::atomic::Ordering::SeqCst) != gen {
                        emit(&Event::new("info").with(
                            "message",
                            json!("Ignoring a superseded OAuth attempt (a newer /login is in progress)."),
                        ));
                        return;
                    }
                    match outcome {
                        Ok(oauth::LoginOutcome::Done) => {
                            finalize_oauth(&st, &cl, &preset, &label).await;
                        }
                        Ok(oauth::LoginOutcome::AwaitingCode { pending }) => {
                            *st.pending_oauth.lock().await = Some(pending);
                            emit(&Event::new("info").with(
                                "message",
                                json!("OAuth login awaiting a code. Open the URL above on any device, approve, then paste the code via /oauth-code <code>."),
                            ));
                        }
                        Err(e) => {
                            st.logger.log(
                                "login_oauth_error",
                                json!({ "provider": preset, "error": e }),
                            );
                            emit(
                                &Event::new("error")
                                    .with("message", json!(format!("OAuth login failed: {e}"))),
                            );
                        }
                    }
                }));
            }
            Command::OauthCode { code } => {
                // Complete a pending plugin OAuth login (manual / device paste).
                let pending = state.pending_oauth.lock().await.take();
                let pending = match pending {
                    Some(p) => p,
                    None => {
                        emit(&Event::new("error").with(
                            "message",
                            json!("No pending OAuth login. Run /login first — the no-browser flow prints a URL; paste its code here with /oauth-code <code>."),
                        ));
                        continue;
                    }
                };
                let preset = pending.kind.clone();
                let label = state
                    .plugin_manager
                    .oauth_config(&preset)
                    .map(|c| c.label)
                    .unwrap_or_else(|| preset.clone());
                if state.plugin_manager.oauth_config(&preset).is_none() {
                    emit(&Event::new("error").with(
                        "message",
                        json!(format!(
                            "no plugin OAuth provider for '{preset}' — pending login discarded"
                        )),
                    ));
                    continue;
                }
                let result = state
                    .plugin_manager
                    .oauth_complete(&preset, &pending, &code)
                    .await;
                match result {
                    Ok(()) => {
                        finalize_oauth(&state, &client, &preset, &label).await;
                    }
                    Err(e) => {
                        // Restore the pending state so the user can retry with a
                        // corrected code without restarting /login.
                        *state.pending_oauth.lock().await = Some(pending);
                        emit(&Event::new("error").with(
                            "message",
                            json!(format!(
                                "OAuth code exchange failed: {e} (pending login restored — try /oauth-code again with the correct code)"
                            )),
                        ));
                    }
                }
            }
            Command::SetApproval { mode } => {
                let Some(new) = Approval::try_parse(&mode) else {
                    emit(&Event::new("error").with(
                        "message",
                        json!(format!(
                            "unknown approval mode '{mode}' (use never|destructive|always)"
                        )),
                    ));
                    continue;
                };
                state.cfg.write().await.approval = new.clone();
                state
                    .logger
                    .log("set_approval", json!({ "mode": new.as_str() }));
                let snap = state.cfg.read().await.clone();
                let persist_err = config::save_runtime_knobs(&snap)
                    .err()
                    .map(|e| e.to_string());
                emit(
                    &Event::new("approval_changed")
                        .with("mode", json!(new.as_str()))
                        .with("ephemeral", json!(false))
                        .with("persisted", json!(persist_err.is_none())),
                );
                if let Some(e) = persist_err {
                    emit(&Event::new("error").with(
                        "message",
                        json!(format!(
                            "approval changed in-memory but failed to persist: {e}"
                        )),
                    ));
                }
            }
            Command::SetConfig { key, value } => {
                // Minimal runtime knob setter for the values the TUI settings
                // modal edits. Coerce string-or-number to u64, string-or-bool
                // to bool.
                let as_u64 = |v: &Value| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
                };
                let as_bool = |v: &Value| {
                    v.as_bool().or_else(|| {
                        v.as_str().and_then(|s| match s {
                            "1" | "true" | "on" => Some(true),
                            "0" | "false" | "off" => Some(false),
                            _ => None,
                        })
                    })
                };
                let mut cfg = state.cfg.write().await;
                let out_key = key.clone();
                let mut out_val = value.clone();
                let mut applied = false;
                match key.as_str() {
                    "bash_timeout_secs" => {
                        if let Some(n) = as_u64(&value) {
                            cfg.bash_timeout_secs = n;
                            out_val = json!(n);
                            applied = true;
                        }
                    }
                    "auto_compact" => {
                        if let Some(b) = as_bool(&value) {
                            cfg.auto_compact = b;
                            out_val = json!(b);
                            applied = true;
                        }
                    }
                    "advisor.enabled" => {
                        if let Some(b) = as_bool(&value) {
                            cfg.advisor.enabled = b;
                            out_val = json!(b);
                            applied = true;
                        }
                    }
                    "advisor.subagents" => {
                        if let Some(b) = as_bool(&value) {
                            cfg.advisor.subagents = b;
                            out_val = json!(b);
                            applied = true;
                        }
                    }
                    "advisor.nudge" => {
                        if let Some(b) = as_bool(&value) {
                            cfg.advisor.nudge = b;
                            out_val = json!(b);
                            applied = true;
                        }
                    }
                    "advisor.model" => {
                        if let Some(s) = value.as_str() {
                            cfg.advisor.model = (!s.trim().is_empty()).then(|| s.to_string());
                            out_val = json!(cfg.advisor.model);
                            applied = true;
                        }
                    }
                    "advisor.subagent_model" => {
                        if let Some(s) = value.as_str() {
                            cfg.advisor.subagent_model =
                                (!s.trim().is_empty()).then(|| s.to_string());
                            out_val = json!(cfg.advisor.subagent_model);
                            applied = true;
                        }
                    }
                    "sandbox" => {
                        let mode = value.as_str().map(String::from).or_else(|| {
                            value.as_bool().map(|b| {
                                if b {
                                    "microsandbox".into()
                                } else {
                                    "none".into()
                                }
                            })
                        });
                        if let Some(mode) = mode {
                            cfg.sandbox = config::Sandbox::parse(&mode);
                            out_val = json!(cfg.sandbox.as_str());
                            applied = true;
                        }
                    }
                    _ => {
                        drop(cfg);
                        emit(
                            &Event::new("error")
                                .with("message", json!(format!("unknown config key: {key}"))),
                        );
                        continue;
                    }
                }
                if !applied {
                    drop(cfg);
                    emit(&Event::new("error").with(
                        "message",
                        json!(format!(
                            "set_config: could not coerce value for key '{out_key}'"
                        )),
                    ));
                    continue;
                }
                state
                    .logger
                    .log("set_config", json!({ "key": out_key, "value": out_val }));
                // Snapshot for persist before drop.
                let snap = cfg.clone();
                drop(cfg);
                let persist_err = config::save_runtime_knobs(&snap)
                    .err()
                    .map(|e| e.to_string());
                emit(
                    &Event::new("config_changed")
                        .with("key", json!(out_key))
                        .with("value", json!(out_val))
                        .with("ephemeral", json!(false))
                        .with("persisted", json!(persist_err.is_none())),
                );
                if let Some(e) = persist_err {
                    emit(&Event::new("error").with(
                        "message",
                        json!(format!(
                            "config '{out_key}' applied in-memory but failed to persist: {e}"
                        )),
                    ));
                }
            }
            Command::GetSandboxStatus => {
                let report = crate::sandbox::sandbox_status().await;
                emit(
                    &Event::new("sandbox_status")
                        .with("mode", json!(state.cfg.read().await.sandbox.as_str()))
                        .with("report", serde_json::to_value(&report).unwrap_or_default()),
                );
            }
            Command::PrepareSandbox => {
                // Off the command loop — runtime download can take minutes and
                // must not freeze abort/approve/send (CORE_REVIEW).
                emit(
                    &Event::new("sandbox_prepare_progress")
                        .with("phase", json!("downloading-runtime-and-image")),
                );
                tokio::spawn(async move {
                    match crate::sandbox::prepare_sandbox().await {
                        Ok(()) => {
                            let report = crate::sandbox::sandbox_status().await;
                            emit(
                                &Event::new("sandbox_ready")
                                    .with("ready", json!(report.ready))
                                    .with(
                                        "report",
                                        serde_json::to_value(&report).unwrap_or_default(),
                                    ),
                            );
                        }
                        Err(e) => {
                            emit(
                                &Event::new("sandbox_error").with("error", json!(e.user_message())),
                            );
                        }
                    }
                });
            }
            Command::ResetSandbox => match crate::sandbox::reset_sandbox().await {
                Ok(()) => {
                    let report = crate::sandbox::sandbox_status().await;
                    emit(
                        &Event::new("sandbox_status")
                            .with("mode", json!(state.cfg.read().await.sandbox.as_str()))
                            .with("reset", json!(true))
                            .with("report", serde_json::to_value(&report).unwrap_or_default()),
                    );
                }
                Err(e) => {
                    emit(
                        &Event::new("sandbox_error")
                            .with("error", json!(e.user_message()))
                            .with("reset", json!(true)),
                    );
                }
            },
            Command::Reset => {
                cancel_in_flight_turn(&state, CancellationReason::Reset, false).await;
                state.conversation.lock().await.clear();
                state.pending_bash.lock().await.clear();
                state.enabled_deferred_tools.lock().await.clear();
                state.eval_history.lock().await.clear();
                state.tool_output_cache.lock().await.invalidate_all();
                let cfg = state.cfg.read().await;
                if let Some(p) = cfg.session_file.as_ref() {
                    session::rewrite(p, &[]);
                }
                state.invalidate_real_token_baseline().await;
                clear_work_state(&state).await;
                reset_stats(&state).await;
                emit(&Event::new("reset"));
            }
            Command::Clear => {
                // In-memory only: keep the session file so a restart can still resume.
                cancel_in_flight_turn(&state, CancellationReason::Clear, false).await;
                state.conversation.lock().await.clear();
                state.pending_bash.lock().await.clear();
                state.enabled_deferred_tools.lock().await.clear();
                state.eval_history.lock().await.clear();
                state.tool_output_cache.lock().await.invalidate_all();
                state.invalidate_real_token_baseline().await;
                clear_work_state(&state).await;
                reset_stats(&state).await;
                emit(&Event::new("cleared").with("persist", json!(true)));
            }
            Command::Undo => {
                // Count for telemetry (session_stop human_corrections).
                state
                    .undo_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Restore filesystem from the latest auto checkpoint (if any)
                // BEFORE popping conversation, so the user gets both back.
                {
                    let cfg = state.cfg.read().await;
                    let _ = checkpoint::restore_latest_auto(
                        &cfg.workspace,
                        cfg.session_file.as_deref(),
                    );
                }
                // Drop the last turn: a user msg + everything after it (assistant, tool msgs).
                let mut conv = state.conversation.lock().await;
                // Capture the undone task (last user message) before dropping it,
                // to record it as a rejected approach for future planning.
                let undone_task: Option<String> = conv
                    .iter()
                    .rev()
                    .find(|m| m.is_user())
                    .and_then(|m| m.content_text().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty());
                // Walk back past trailing tool/assistant messages to the last user message.
                while let Some(last) = conv.last() {
                    if last.is_user() {
                        conv.pop();
                        break;
                    }
                    conv.pop();
                }
                if let Some(p) = state.cfg.read().await.session_file.as_ref() {
                    session::rewrite(p, &conv);
                }
                // Replay remaining history so the TUI rebuilds the transcript
                // (trimmed last turn only) instead of wiping the whole view.
                let visible: Vec<Value> = conv
                    .iter()
                    .filter(|m| !m.is_system())
                    .map(Value::from)
                    .collect();
                let est = estimate_messages_tokens(&conv);
                drop(conv);
                // Record the undone task as a rejected approach (implicit user
                // rejection of the agent's work) so future similar tasks can warn
                // the planner. Moderate confidence — an undo is implicit, not an
                // explicit "this approach is wrong" statement. Fail-open.
                if let Some(task) = undone_task {
                    let cfg = state.cfg.read().await;
                    let pid = project_identity::resolve_project_identity(&cfg.workspace).id;
                    let fp =
                        task_fingerprint::build_fingerprint(&task_fingerprint::FingerprintInputs {
                            user_intent: &task,
                            files_read: &[],
                            files_changed: &[],
                            symbols: &[],
                            tools_used: &[],
                            diagnostics: &[],
                            tests_run: &[],
                        });
                    let approach: String = task.chars().take(200).collect();
                    let record = rejected_approaches::RejectedApproach {
                        id: format!(
                            "undo-{}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis())
                                .unwrap_or(0)
                        ),
                        scope: "project".into(),
                        task_fingerprint: fp,
                        approach: format!("agent implementation reverted by /undo: {approach}"),
                        rejection_reason:
                            "user undid the agent's changes (reverted to the last checkpoint)"
                                .into(),
                        preferred_alternative: None,
                        evidence: vec![],
                        confidence: rejected_approaches::confidence_for_rejection(false, 1, false),
                        status: preferences::LearningStatus::Candidate,
                    };
                    rejected_approaches::append_rejected(Some(&pid), &record);
                }
                // The dropped turn invalidates the real baseline's length anchor.
                state.invalidate_real_token_baseline().await;
                *state.estimated_tokens.lock().await = est;
                clear_work_state(&state).await;
                emit(
                    &Event::new("history")
                        .with("messages", json!(visible))
                        .with("tokens_in", json!(est)),
                );
            }
            Command::CreateCheckpoint { label, paths } => {
                let cfg = state.cfg.read().await;
                let label = label.unwrap_or_else(|| "manual".into());
                let paths = paths.unwrap_or_default();
                match checkpoint::create(
                    &cfg.workspace,
                    cfg.session_file.as_deref(),
                    &label,
                    &paths,
                    false,
                ) {
                    Ok(m) => emit(&Event::new("info").with(
                        "message",
                        json!(format!("checkpoint {} created ({})", m.id, m.kind)),
                    )),
                    Err(e) => emit(&Event::new("error").with("message", json!(e))),
                }
            }
            Command::ListCheckpoints => {
                let cfg = state.cfg.read().await;
                let index = checkpoint::index_path(cfg.session_file.as_deref(), &cfg.workspace);
                let metas = checkpoint::list(&index);
                emit(&Event::new("checkpoints").with("checkpoints", json!(metas)));
            }
            Command::RestoreCheckpoint { id } => {
                let cfg = state.cfg.read().await;
                match checkpoint::restore(&cfg.workspace, cfg.session_file.as_deref(), &id) {
                    Ok(m) => emit(&Event::new("info").with(
                        "message",
                        json!(format!("restored checkpoint {} ({})", m.id, m.kind)),
                    )),
                    Err(e) => emit(&Event::new("error").with("message", json!(e))),
                }
            }
            Command::Compact { instructions } => {
                // Off the command loop — provider summarize can take tens of
                // seconds and must not freeze abort/approve/send (CORE_REVIEW).
                let st = state.clone();
                let client_c = client.clone();
                tokio::spawn(async move {
                    let mut messages = st.conversation.lock().await.clone();
                    if messages.len() <= 2 {
                        emit(&Event::new("info").with("message", json!("nothing to compact yet")));
                        return;
                    }
                    dispatch_lifecycle(&st, "pre_compact").await;
                    let before_est = estimate_messages_tokens(&messages);
                    let (model_ctx, model_max_tokens) = {
                        let last = st.last_model.lock().await.clone();
                        let models = st.models.read().await;
                        last.as_deref()
                            .and_then(|m| models.iter().find(|mi| mi.id == m))
                            .map(|m| (m.context_window as u64, m.max_tokens))
                            .unwrap_or((200_000, 8_192))
                    };
                    let cfg = st.cfg.read().await.clone();
                    let policy = context_policy(
                        &messages,
                        model_ctx,
                        model_max_tokens,
                        cfg.context_compact_at,
                        cfg.context_digest_at,
                    );
                    emit(
                        &Event::new("compacting")
                            .with("before_tokens", json!(before_est))
                            .with("trigger", json!("manual"))
                            .with("context_window", json!(model_ctx))
                            .with("threshold_tokens", json!(policy.compact_threshold))
                            .with("hard_limit_tokens", json!(policy.hard_limit))
                            .with(
                                "utilization_pct",
                                json!(utilization_pct(before_est, model_ctx)),
                            ),
                    );
                    let model_name = st.last_model.lock().await.clone().unwrap_or_default();
                    let rp = st.resolve_provider_for_model(&model_name).await;
                    let instr_owned: Option<String> = match instructions
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        Some(s) => Some(s.to_string()),
                        None => cfg.compact_instructions.clone(),
                    };
                    let cancel = CancellationToken::new();
                    let summary_chars = if rp.api_key.is_some() && !model_name.is_empty() {
                        let mp = st.plugin_manager.memory_provider();
                        compact_with_summary(
                            &client_c,
                            &cfg,
                            &rp,
                            &model_name,
                            &mut messages,
                            &cancel,
                            false,
                            model_ctx,
                            instr_owned.as_deref(),
                            mp.as_ref(),
                        )
                        .await
                    } else {
                        compact_conversation(&mut messages, model_ctx);
                        0
                    };
                    *st.conversation.lock().await = messages.clone();
                    let after_est = estimate_messages_tokens(&messages);
                    *st.estimated_tokens.lock().await = after_est;
                    st.invalidate_real_token_baseline().await;
                    if let Some(p) = st.cfg.read().await.session_file.as_ref() {
                        let retained = session::artifact_run_ids_referenced_by_messages(&messages);
                        let artifacts: Vec<String> = retained
                            .iter()
                            .map(|id| format!("artifact://{id}.json"))
                            .collect();
                        if let Err(error) = session::append_compaction(
                            p,
                            &messages,
                            "manual compaction",
                            &artifacts,
                        ) {
                            emit(&Event::new("error").with("message", json!(error)));
                        } else if let Err(error) = session::shake_artifacts(p, &retained) {
                            emit(&Event::new("error").with("message", json!(error)));
                        }
                    }
                    emit(
                        &Event::new("compacted")
                            .with("before_tokens", json!(before_est))
                            .with("after_tokens", json!(after_est))
                            .with("summary_chars", json!(summary_chars))
                            .with("context_window", json!(model_ctx))
                            .with("threshold_tokens", json!(policy.compact_threshold))
                            .with("hard_limit_tokens", json!(policy.hard_limit))
                            .with("within_limit", json!(after_est <= policy.hard_limit)),
                    );
                    st.compaction_count
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if after_est > policy.hard_limit {
                        emit(&Event::new("error").with(
                            "message",
                            json!(format!(
                                "context remains too large after compaction ({after_est} > safe limit {}); remove or split an oversized recent message",
                                policy.hard_limit
                            )),
                        ));
                    }
                });
            }
            Command::ListSessions => {
                let (dir, current_name) = {
                    let cfg = state.cfg.read().await;
                    let sf = cfg.session_file.as_ref();
                    let dir = sf
                        .and_then(|p| p.parent().map(|x| x.to_path_buf()))
                        .unwrap_or_else(|| std::path::PathBuf::from("."));
                    let cur = sf.and_then(|p| p.file_name()).map(|n| n.to_os_string());
                    (dir, cur)
                };
                let mut entries: Vec<Value> = Vec::new();
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    for e in rd.flatten() {
                        let path = e.path();
                        if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                            continue;
                        }
                        let name = e.file_name().to_string_lossy().to_string();
                        let info = session::describe(&path);
                        let meta = session::read_meta(&path);
                        let mtime = e
                            .metadata()
                            .ok()
                            .and_then(|m| m.modified().ok())
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        let current = current_name
                            .as_ref()
                            .map(|n| *n == e.file_name())
                            .unwrap_or(false);
                        let title = meta
                            .title
                            .or(info.title)
                            .unwrap_or_else(|| "(no messages yet)".to_string());
                        entries.push(json!({
                            "name": name,
                            "path": path.display().to_string(),
                            "title": title,
                            "messages": info.messages,
                            "mtime": mtime,
                            "current": current,
                            "pinned": meta.pinned,
                        }));
                    }
                }
                // Most recently modified first.
                entries.sort_by(|a, b| {
                    b["pinned"]
                        .as_bool()
                        .unwrap_or(false)
                        .cmp(&a["pinned"].as_bool().unwrap_or(false))
                        .then_with(|| {
                            b["mtime"]
                                .as_u64()
                                .unwrap_or(0)
                                .cmp(&a["mtime"].as_u64().unwrap_or(0))
                        })
                });
                let files: Vec<String> = entries
                    .iter()
                    .filter_map(|e| e["name"].as_str().map(|s| s.to_string()))
                    .collect();
                emit(
                    &Event::new("sessions")
                        .with("sessions", json!(entries))
                        .with("files", json!(files)),
                );
            }
            Command::LoadSession { path } => {
                let mut p = std::path::PathBuf::from(&path);
                // Resolve relative paths against the sessions dir so the picker
                // (which may send a bare filename) works.
                if !p.is_absolute() {
                    if let Some(sess_dir) = state
                        .cfg
                        .read()
                        .await
                        .session_file
                        .as_ref()
                        .and_then(|sf| sf.parent())
                    {
                        p = sess_dir.join(&p);
                    }
                }

                // Confine session paths under the sessions directory (CORE_REVIEW).
                if let Some(root) = state
                    .cfg
                    .read()
                    .await
                    .session_file
                    .as_ref()
                    .and_then(|sf| sf.parent().map(|p| p.to_path_buf()))
                {
                    let canon_root = root.canonicalize().unwrap_or_else(|_| root.clone());
                    let canon_p = p.canonicalize().unwrap_or_else(|_| p.clone());
                    if !canon_p.starts_with(&canon_root) {
                        emit(&Event::new("error").with(
                            "message",
                            json!(format!(
                                "session path escapes sessions directory: {}",
                                p.display()
                            )),
                        ));
                        continue;
                    }
                    p = canon_p;
                } else if p.is_absolute() {
                    emit(&Event::new("error").with(
                        "message",
                        json!(
                            "refusing absolute session path without an active sessions directory"
                        ),
                    ));
                    continue;
                }
                let loaded = match session::load(&p) {
                    Ok(v) => v,
                    Err(e) => {
                        emit(
                            &Event::new("session_change_failed")
                                .with("path", json!(path))
                                .with("message", json!(e)),
                        );
                        continue;
                    }
                };
                cancel_in_flight_turn(&state, CancellationReason::LoadSession, true).await;
                *state.conversation.lock().await = loaded.clone();
                state.pending_bash.lock().await.clear();
                state.enabled_deferred_tools.lock().await.clear();
                state.eval_history.lock().await.clear();
                state.tool_output_cache.lock().await.invalidate_all();
                // Restore the loaded session's cumulative stats so `/stats` shows
                // its real totals, not the prior session's.
                restore_stats(&state, &p).await;
                // Point the session_file at the loaded path so future appends go there.
                state.cfg.write().await.session_file = Some(p.clone());
                emit(
                    &Event::new("session_changed")
                        .with("path", json!(p.display().to_string()))
                        .with("new", json!(false)),
                );
                emit(&Event::new("reset"));
                // Replay the loaded transcript so the TUI shows prior turns
                // instead of an empty view after switching/resuming a session.
                let visible: Vec<Value> = loaded
                    .iter()
                    .filter(|m| !m.is_system())
                    .map(Value::from)
                    .collect();
                let est = estimate_messages_tokens(&loaded);
                *state.estimated_tokens.lock().await = est;
                // Loaded history has no known real token count yet; the next
                // request's `usage` will re-establish the baseline.
                state.invalidate_real_token_baseline().await;
                clear_work_state(&state).await;
                emit(
                    &Event::new("history")
                        .with("messages", json!(visible))
                        .with("tokens_in", json!(est)),
                );
                emit(&Event::new("info").with(
                    "message",
                    json!(format!("loaded {} messages from {}", loaded.len(), path)),
                ));
            }
            Command::RenameSession { path, title } => {
                let mut p = std::path::PathBuf::from(&path);
                if !p.is_absolute() {
                    if let Some(dir) = state
                        .cfg
                        .read()
                        .await
                        .session_file
                        .as_ref()
                        .and_then(|x| x.parent())
                    {
                        p = dir.join(&p);
                    }
                }

                // Confine session paths under the sessions directory (CORE_REVIEW).
                if let Some(root) = state
                    .cfg
                    .read()
                    .await
                    .session_file
                    .as_ref()
                    .and_then(|sf| sf.parent().map(|d| d.to_path_buf()))
                {
                    let canon_root = root.canonicalize().unwrap_or_else(|_| root.clone());
                    let canon_p = p.canonicalize().unwrap_or_else(|_| p.clone());
                    if !canon_p.starts_with(&canon_root) {
                        emit(&Event::new("error").with(
                            "message",
                            json!(format!(
                                "session path escapes sessions directory: {}",
                                p.display()
                            )),
                        ));
                        continue;
                    }
                    p = canon_p;
                } else if p.is_absolute() {
                    emit(&Event::new("error").with(
                        "message",
                        json!(
                            "refusing absolute session path without an active sessions directory"
                        ),
                    ));
                    continue;
                }
                let title = title.trim();
                if title.is_empty() {
                    emit(
                        &Event::new("error")
                            .with("message", json!("session title cannot be empty")),
                    );
                    continue;
                }
                let title = title.chars().take(120).collect::<String>();
                match session::update_meta(&p, |meta| meta.title = Some(title.clone())) {
                    Ok(_) => emit(
                        &Event::new("session_renamed")
                            .with("path", json!(path))
                            .with("title", json!(title)),
                    ),
                    Err(e) => emit(
                        &Event::new("error")
                            .with("message", json!(format!("rename session failed: {e}"))),
                    ),
                }
            }
            Command::PinSession { path, pinned } => {
                let mut p = std::path::PathBuf::from(&path);
                if !p.is_absolute() {
                    if let Some(dir) = state
                        .cfg
                        .read()
                        .await
                        .session_file
                        .as_ref()
                        .and_then(|x| x.parent())
                    {
                        p = dir.join(&p);
                    }
                }

                // Confine session paths under the sessions directory (CORE_REVIEW).
                if let Some(root) = state
                    .cfg
                    .read()
                    .await
                    .session_file
                    .as_ref()
                    .and_then(|sf| sf.parent().map(|d| d.to_path_buf()))
                {
                    let canon_root = root.canonicalize().unwrap_or_else(|_| root.clone());
                    let canon_p = p.canonicalize().unwrap_or_else(|_| p.clone());
                    if !canon_p.starts_with(&canon_root) {
                        emit(&Event::new("error").with(
                            "message",
                            json!(format!(
                                "session path escapes sessions directory: {}",
                                p.display()
                            )),
                        ));
                        continue;
                    }
                    p = canon_p;
                } else if p.is_absolute() {
                    emit(&Event::new("error").with(
                        "message",
                        json!(
                            "refusing absolute session path without an active sessions directory"
                        ),
                    ));
                    continue;
                }
                match session::update_meta(&p, |meta| meta.pinned = pinned) {
                    Ok(_) => emit(
                        &Event::new("session_pinned")
                            .with("path", json!(path))
                            .with("pinned", json!(pinned)),
                    ),
                    Err(e) => emit(
                        &Event::new("error")
                            .with("message", json!(format!("pin session failed: {e}"))),
                    ),
                }
            }
            Command::DeleteSession { path } => {
                let mut p = std::path::PathBuf::from(&path);
                if !p.is_absolute() {
                    if let Some(dir) = state
                        .cfg
                        .read()
                        .await
                        .session_file
                        .as_ref()
                        .and_then(|x| x.parent())
                    {
                        p = dir.join(&p);
                    }
                }

                // Confine session paths under the sessions directory (CORE_REVIEW).
                if let Some(root) = state
                    .cfg
                    .read()
                    .await
                    .session_file
                    .as_ref()
                    .and_then(|sf| sf.parent().map(|d| d.to_path_buf()))
                {
                    let canon_root = root.canonicalize().unwrap_or_else(|_| root.clone());
                    let canon_p = p.canonicalize().unwrap_or_else(|_| p.clone());
                    if !canon_p.starts_with(&canon_root) {
                        emit(&Event::new("error").with(
                            "message",
                            json!(format!(
                                "session path escapes sessions directory: {}",
                                p.display()
                            )),
                        ));
                        continue;
                    }
                    p = canon_p;
                } else if p.is_absolute() {
                    emit(&Event::new("error").with(
                        "message",
                        json!(
                            "refusing absolute session path without an active sessions directory"
                        ),
                    ));
                    continue;
                }
                let current = state.cfg.read().await.session_file.clone();
                if current.as_ref() == Some(&p) {
                    emit(&Event::new("error").with(
                        "message",
                        json!("cannot delete the active session; start or load another first"),
                    ));
                    continue;
                }
                match session::delete_with_sidecars(&p) {
                    Ok(()) => emit(&Event::new("session_deleted").with("path", json!(path))),
                    Err(e) => emit(
                        &Event::new("error")
                            .with("message", json!(format!("delete session failed: {e}"))),
                    ),
                }
            }
            Command::NewSession { path } => {
                // Stop any in-flight turn so it can't keep writing into the new session.
                cancel_in_flight_turn(&state, CancellationReason::NewSession, true).await;
                // Start a fresh session file in the same project dir. The old
                // file is left on disk so sessions accumulate per project.
                let new_path = match path {
                    Some(name) => {
                        let mut p = std::path::PathBuf::from(name);
                        if !p.is_absolute() {
                            if let Some(sess_dir) = state
                                .cfg
                                .read()
                                .await
                                .session_file
                                .as_ref()
                                .and_then(|sf| sf.parent())
                            {
                                p = sess_dir.join(&p);
                            }
                        }
                        p
                    }
                    None => {
                        let dir = state
                            .cfg
                            .read()
                            .await
                            .session_file
                            .as_ref()
                            .and_then(|p| p.parent().map(|x| x.to_path_buf()))
                            .unwrap_or_else(|| std::path::PathBuf::from("."));
                        dir.join(new_session_filename())
                    }
                };
                session::ensure(&new_path);
                *state.conversation.lock().await = Vec::new();
                state.pending_bash.lock().await.clear();
                state.enabled_deferred_tools.lock().await.clear();
                state.eval_history.lock().await.clear();
                state.tool_output_cache.lock().await.invalidate_all();
                state.invalidate_real_token_baseline().await;
                clear_work_state(&state).await;
                state.cfg.write().await.session_file = Some(new_path.clone());
                // Fresh session: zero the cumulative stats (in memory + sidecar).
                reset_stats(&state).await;
                state
                    .undo_count
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                state
                    .skill_read_count
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                state.logger.log(
                    "new_session",
                    json!({ "path": new_path.display().to_string() }),
                );
                emit(
                    &Event::new("session_changed")
                        .with("path", json!(new_path.display().to_string()))
                        .with("new", json!(true)),
                );
                emit(&Event::new("reset"));
                emit(&Event::new("info").with(
                    "message",
                    json!(format!("started new session: {}", new_path.display())),
                ));
            }
            Command::Stats => {
                // Cumulative REAL usage (billing totals — accurate by construction:
                // each turn adds the endpoint's actual prompt/completion tokens).
                let ti = *state.tokens_in.lock().await; // cumulative prompt
                let to = *state.tokens_out.lock().await; // cumulative output
                let cached = *state.cached_tokens.lock().await;
                let turns = state.logger.turn_count();
                let cache_hit_ratio = if ti > 0 {
                    cached as f64 / ti as f64
                } else {
                    0.0
                };
                // `tokens_in` = the CURRENT real context — the SAME grounded
                // estimate the footer uses (real prompt_tokens + small delta) — so
                // /stats "in" matches the footer instead of the cumulative prompt,
                // which re-sums the whole prefix every turn and looks inflated next
                // to it. The cumulative prompt is still exposed as `total_in` for
                // billing and the cache ratio.
                let ctx = {
                    let conv = state.conversation.lock().await;
                    let last_real = *state.last_real_prompt_tokens.lock().await;
                    let len_at = *state.conv_len_at_last_real.lock().await;
                    grounded_estimate(&conv, last_real, len_at)
                };
                let msg_count = state.conversation.lock().await.len();
                let session_file = state
                    .cfg
                    .read()
                    .await
                    .session_file
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                emit(
                    &Event::new("stats")
                        .with("tokens_in", json!(ctx)) // current context (footer match)
                        .with("tokens_out", json!(to)) // cumulative output
                        .with("total_in", json!(ti)) // cumulative prompt (billing)
                        .with("tokens_total", json!(ti + to)) // cumulative in+out
                        .with("cached_tokens", json!(cached))
                        .with("cache_hit_ratio", json!(cache_hit_ratio))
                        .with("turns", json!(turns))
                        .with(
                            "compactions",
                            json!(state
                                .compaction_count
                                .load(std::sync::atomic::Ordering::Relaxed)),
                        )
                        .with("messages", json!(msg_count))
                        .with("session_file", json!(session_file)),
                );
            }
            Command::RuntimeStatus => {
                let snapshot = state.runtime.snapshot();
                let last_cancellation = snapshot.last_cancellation.map(|cancelled| {
                    json!({
                        "session_id": cancelled.session_id,
                        "run_id": cancelled.run_id,
                        "reason": cancelled.reason.as_str(),
                    })
                });
                emit(
                    &Event::new("runtime_status")
                        .with("session_id", json!(snapshot.session_id))
                        .with("run_id", json!(snapshot.run_id))
                        .with(
                            "discarded_stale_results",
                            json!(snapshot.discarded_stale_results),
                        )
                        .with(
                            "compactions",
                            json!(state
                                .compaction_count
                                .load(std::sync::atomic::Ordering::Relaxed)),
                        )
                        .with("last_cancellation", json!(last_cancellation))
                        .with("resources", json!(snapshot.resources))
                        .with("pending_approvals", json!(state.pending.lock().await.len()))
                        .with("pending_asks", json!(state.pending_asks.lock().await.len()))
                        .with(
                            "pending_sudos",
                            json!(state.pending_sudos.lock().await.len()),
                        ),
                );
            }
            Command::Usage { model } => {
                // Off the command loop — usage HTTP must not freeze stdin (CORE_REVIEW).
                let st = state.clone();
                let client_u = client.clone();
                tokio::spawn(async move {
                    let model_name = match model.filter(|m| !m.is_empty()) {
                        Some(m) => m,
                        None => st.last_model.lock().await.clone().unwrap_or_default(),
                    };
                    let model_name = if model_name.is_empty() {
                        st.models
                            .read()
                            .await
                            .first()
                            .map(|m| m.id.clone())
                            .unwrap_or_default()
                    } else {
                        model_name
                    };
                    let rp = if model_name.is_empty() {
                        let rp = st.resolved_provider().await;
                        oauth::enrich_oauth(rp, &client_u, Some(&st.plugin_manager)).await
                    } else {
                        st.resolve_provider_for_model(&model_name).await
                    };
                    let usage = providers::registry::adapter_for(&rp)
                        .usage_status(providers::adapter::ProviderContext {
                            client: &client_u,
                            provider: &rp,
                        })
                        .await;
                    let mut ev = Event::new("usage")
                        .with("provider", json!(rp.name))
                        .with("provider_kind", json!(rp.kind.to_string()))
                        .with("model", json!(model_name))
                        .with("base_url", json!(rp.base_url));
                    for (k, v) in usage.to_event_fields() {
                        ev = ev.with(&k, v);
                    }
                    emit(&ev);
                });
            }
            Command::Context => {
                // Token-breakdown: where is the context window being spent?
                // Aggregates per-message token estimates (same char/4 heuristic
                // the footer uses) so the user can see the biggest consumers
                // before compaction fires. Read-only — never mutates state.
                let conv = state.conversation.lock().await.clone();
                let total = {
                    let last_real = *state.last_real_prompt_tokens.lock().await;
                    let len_at = *state.conv_len_at_last_real.lock().await;
                    grounded_estimate(&conv, last_real, len_at)
                };
                let (model_ctx, model_max_tokens) = {
                    let last = state.last_model.lock().await.clone();
                    let models = state.models.read().await;
                    last.as_deref()
                        .and_then(|m| models.iter().find(|mi| mi.id == m))
                        .map(|m| (m.context_window as u64, m.max_tokens))
                        .unwrap_or((200_000, 8_192))
                };
                let cfg = state.cfg.read().await;
                let policy = context_policy(
                    &conv,
                    model_ctx,
                    model_max_tokens,
                    cfg.context_compact_at,
                    cfg.context_digest_at,
                );
                let pct = if model_ctx > 0 {
                    (total as f64 / model_ctx as f64 * 100.0).round() as u64
                } else {
                    0
                };
                // Per-message estimates; role buckets are aggregated below from
                // the entries for clean u64 values.
                let mut entries: Vec<Value> = Vec::with_capacity(conv.len());
                for (i, m) in conv.iter().enumerate() {
                    let tokens = estimate_messages_tokens(std::slice::from_ref(m));
                    let role = m.role();
                    let preview: String = m
                        .content_text()
                        .map(|t| {
                            let t = t.replace('\n', " ");
                            if t.chars().count() > 100 {
                                format!("{}…", t.chars().take(100).collect::<String>())
                            } else {
                                t
                            }
                        })
                        .unwrap_or_else(|| "(no text / multimodal)".to_string());
                    entries.push(json!({
                        "index": i,
                        "role": role,
                        "tokens": tokens,
                        "preview": preview,
                    }));
                }
                // Aggregate per-role token totals.
                let role_obj: Value = {
                    let mut counts: std::collections::BTreeMap<String, u64> =
                        std::collections::BTreeMap::new();
                    for e in &entries {
                        let r = e["role"].as_str().unwrap_or("").to_string();
                        let t = e["tokens"].as_u64().unwrap_or(0);
                        *counts.entry(r).or_insert(0) += t;
                    }
                    let mut map = serde_json::Map::new();
                    for (k, v) in counts {
                        map.insert(k, json!(v));
                    }
                    Value::Object(map)
                };
                let system_tokens = entries
                    .iter()
                    .filter(|e| e["role"].as_str() == Some("system"))
                    .map(|e| e["tokens"].as_u64().unwrap_or(0))
                    .sum::<u64>();
                // Top 10 consumers by tokens (descending).
                entries.sort_by(|a, b| b["tokens"].as_u64().cmp(&a["tokens"].as_u64()));
                let top: Vec<Value> = entries.iter().take(10).cloned().collect();
                emit(
                    &Event::new("context_breakdown")
                        .with("total_tokens", json!(total))
                        .with("context_window", json!(model_ctx))
                        .with("pct", json!(pct))
                        .with("digest_threshold_tokens", json!(policy.digest_threshold))
                        .with("compact_threshold_tokens", json!(policy.compact_threshold))
                        .with("hard_limit_tokens", json!(policy.hard_limit))
                        .with("response_reserve_tokens", json!(policy.response_reserve))
                        .with("safety_margin_tokens", json!(policy.safety_margin))
                        .with("messages", json!(conv.len()))
                        .with("system_tokens", json!(system_tokens))
                        .with("by_role", role_obj)
                        .with("top_consumers", json!(top)),
                );
            }
            Command::InstallPlugin { path, scope } => {
                let scope = match plugins::PluginInstallScope::parse(
                    scope.as_deref().unwrap_or("global"),
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        emit(
                            &Event::new("plugin_error")
                                .with("name", json!(path))
                                .with("message", json!(e)),
                        );
                        continue;
                    }
                };
                match state.plugin_manager.install_source(&path, scope).await {
                    Ok(plugin) => {
                        let hooks_list: Vec<String> = plugin.hooks.keys().cloned().collect();
                        emit(
                            &Event::new("plugin_installed")
                                .with("name", json!(plugin.name))
                                .with("version", json!(plugin.version))
                                .with("description", json!(plugin.description))
                                .with("hooks", json!(hooks_list))
                                .with("scope", json!(scope.as_str()))
                                .with("path", json!(plugin.source_path.display().to_string())),
                        );
                    }
                    Err(e) => {
                        emit(
                            &Event::new("plugin_error")
                                .with("name", json!(path))
                                .with("message", json!(e)),
                        );
                    }
                }
            }
            Command::RemovePlugin { name } => match state.plugin_manager.remove(&name) {
                Ok(()) => emit(&Event::new("plugin_removed").with("name", json!(name))),
                Err(e) => emit(
                    &Event::new("plugin_error")
                        .with("name", json!(name))
                        .with("message", json!(e)),
                ),
            },
            Command::EnablePlugin { name } => match state.plugin_manager.enable(&name) {
                Ok(()) => emit(&Event::new("plugin_enabled").with("name", json!(name))),
                Err(e) => emit(
                    &Event::new("plugin_error")
                        .with("name", json!(name))
                        .with("message", json!(e)),
                ),
            },
            Command::DisablePlugin { name } => match state.plugin_manager.disable(&name) {
                Ok(()) => emit(&Event::new("plugin_disabled").with("name", json!(name))),
                Err(e) => emit(
                    &Event::new("plugin_error")
                        .with("name", json!(name))
                        .with("message", json!(e)),
                ),
            },
            Command::ListPlugins => {
                let plugins = state.plugin_manager.list();
                let mut entries: Vec<Value> = plugins
                    .values()
                    .map(|p| {
                        let mut hooks: Vec<String> = p.hooks.keys().cloned().collect();
                        hooks.sort();
                        let scope = state.plugin_manager.scope_of_path(&p.source_path);
                        let tools: Vec<String> = p.tools.iter().map(|t| t.name.clone()).collect();
                        let commands: Vec<String> =
                            p.commands.iter().map(|c| c.name.clone()).collect();
                        json!({
                            "name": p.name,
                            "version": p.version,
                            "enabled": p.enabled,
                            "description": p.description,
                            "hooks": hooks,
                            "tools": tools,
                            "commands": commands,
                            "disable_tools": p.disable_tools,
                            "has_oauth": p.oauth.is_some(),
                            "has_memory_provider": p.memory_provider.is_some(),
                            "has_system_prompt": !p.system_prompt.trim().is_empty(),
                            "path": p.source_path.display().to_string(),
                            "scope": scope.as_str(),
                        })
                    })
                    .collect();
                entries.sort_by(|a, b| {
                    a.get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .cmp(b.get("name").and_then(|v| v.as_str()).unwrap_or(""))
                });
                emit(&Event::new("plugins_list").with("plugins", json!(entries)));
                let cmds = state.plugin_manager.command_definitions();
                emit(&Event::new("plugin_commands").with("commands", json!(cmds)));
            }
            Command::ListPluginCommands => {
                let cmds = state.plugin_manager.command_definitions();
                emit(&Event::new("plugin_commands").with("commands", json!(cmds)));
            }
            Command::ReloadPlugins => {
                let summary = state.plugin_manager.reload();
                // Re-apply config plugins_disabled after reload (CORE_REVIEW).
                {
                    let disabled = state.cfg.read().await.plugins_disabled.clone();
                    for name in &disabled {
                        let _ = state.plugin_manager.disable(name);
                    }
                }
                let plugins = state.plugin_manager.list();
                let mut entries: Vec<Value> = plugins
                    .values()
                    .map(|p| {
                        let mut hooks: Vec<String> = p.hooks.keys().cloned().collect();
                        hooks.sort();
                        let scope = state.plugin_manager.scope_of_path(&p.source_path);
                        let tools: Vec<String> = p.tools.iter().map(|t| t.name.clone()).collect();
                        let commands: Vec<String> =
                            p.commands.iter().map(|c| c.name.clone()).collect();
                        json!({
                            "name": p.name,
                            "version": p.version,
                            "enabled": p.enabled,
                            "description": p.description,
                            "hooks": hooks,
                            "tools": tools,
                            "commands": commands,
                            "disable_tools": p.disable_tools,
                            "has_oauth": p.oauth.is_some(),
                            "has_memory_provider": p.memory_provider.is_some(),
                            "has_system_prompt": !p.system_prompt.trim().is_empty(),
                            "path": p.source_path.display().to_string(),
                            "scope": scope.as_str(),
                        })
                    })
                    .collect();
                entries.sort_by(|a, b| {
                    a.get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .cmp(b.get("name").and_then(|v| v.as_str()).unwrap_or(""))
                });
                emit(&Event::new("plugins_list").with("plugins", json!(entries)));
                let cmds = state.plugin_manager.command_definitions();
                emit(&Event::new("plugin_commands").with("commands", json!(cmds)));
                let loaded = summary.get("loaded").and_then(|v| v.as_u64()).unwrap_or(0);
                let skipped = summary
                    .get("skipped")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                let err_n = summary
                    .get("errors")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                emit(&Event::new("info").with(
                    "message",
                    json!(format!(
                        "plugins reloaded: {loaded} loaded, {skipped} skipped, {err_n} errors"
                    )),
                ));
                // Refresh system prompt so plugin injections / memory providers
                // pick up enable/disable / newly loaded plugins.
                let _ = refresh_memory_injection(&state).await;
            }
            Command::PluginTrustPrompt => {
                // The `/plugin-trust` command: re-emit the prompt (includes
                // plugins with a recorded decision so the user can change them).
                let entries = state.plugin_manager.trust_prompt_json();
                emit(&Event::new("plugin_trust_prompt").with("plugins", json!(entries)));
            }
            Command::PluginTrustDecisions { decisions } => {
                let invalid: Vec<String> = decisions
                    .iter()
                    .filter(|(_, v)| v.as_str() != "trust" && v.as_str() != "deny")
                    .map(|(k, _)| k.clone())
                    .collect();
                if !invalid.is_empty() {
                    emit(
                        &Event::new("error").with(
                            "message",
                            json!(format!(
                                "plugin_trust_decisions: invalid value for {} (expected \"trust\" or \"deny\")",
                                invalid.join(", ")
                            )),
                        ),
                    );
                    continue;
                }
                match state
                    .plugin_manager
                    .apply_trust_decisions(decisions.clone())
                {
                    Ok(summary) => {
                        let trusted: Vec<String> = decisions
                            .iter()
                            .filter(|(_, v)| v.as_str() == "trust")
                            .map(|(k, _)| k.clone())
                            .collect();
                        let denied: Vec<String> = decisions
                            .iter()
                            .filter(|(_, v)| v.as_str() == "deny")
                            .map(|(k, _)| k.clone())
                            .collect();
                        let loaded = summary.get("loaded").and_then(|v| v.as_u64()).unwrap_or(0);
                        emit(
                            &Event::new("plugin_trust_applied")
                                .with("trusted", json!(trusted))
                                .with("denied", json!(denied))
                                .with("loaded", json!(loaded)),
                        );
                        // Refresh the plugin registry + slash commands so
                        // newly-trusted plugins are live immediately.
                        let entries: Vec<Value> = state
                            .plugin_manager
                            .list()
                            .values()
                            .map(|p| {
                                let mut hooks: Vec<String> = p.hooks.keys().cloned().collect();
                                hooks.sort();
                                let scope = state.plugin_manager.scope_of_path(&p.source_path);
                                let tools: Vec<String> =
                                    p.tools.iter().map(|t| t.name.clone()).collect();
                                let commands: Vec<String> =
                                    p.commands.iter().map(|c| c.name.clone()).collect();
                                json!({
                                    "name": p.name,
                                    "version": p.version,
                                    "enabled": p.enabled,
                                    "description": p.description,
                                    "hooks": hooks,
                                    "tools": tools,
                                    "commands": commands,
                                    "disable_tools": p.disable_tools,
                                    "has_oauth": p.oauth.is_some(),
                                    "has_memory_provider": p.memory_provider.is_some(),
                                    "has_system_prompt": !p.system_prompt.trim().is_empty(),
                                    "path": p.source_path.display().to_string(),
                                    "scope": scope.as_str(),
                                })
                            })
                            .collect();
                        emit(&Event::new("plugins_list").with("plugins", json!(entries)));
                        let cmds = state.plugin_manager.command_definitions();
                        emit(&Event::new("plugin_commands").with("commands", json!(cmds)));
                        // Refresh system prompt so plugin injections / memory
                        // providers from newly-trusted plugins take effect.
                        let _ = refresh_memory_injection(&state).await;
                    }
                    Err(e) => {
                        emit(&Event::new("error").with("message", json!(e)));
                    }
                }
            }
            Command::PluginCommand { name, args } => {
                match state.plugin_manager.command_config(&name) {
                    Some(cfg) => {
                        let ws = state.cfg.read().await.workspace.display().to_string();
                        let session_id = state
                            .cfg
                            .read()
                            .await
                            .session_file
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .and_then(|n| n.to_str())
                            .unwrap_or("")
                            .to_string();
                        match cfg.mode {
                            plugins::CommandMode::Script => {
                                let out =
                                    plugins::execute_plugin_command(&cfg, &args, &ws, &session_id)
                                        .await;
                                if out.ok {
                                    emit(&Event::new("info").with("message", json!(out.output)));
                                } else {
                                    emit(&Event::new("error").with("message", json!(out.output)));
                                }
                            }
                            plugins::CommandMode::AgentTurn => {
                                // Prompt-backed command: render the template and
                                // submit it as a normal agent turn (the same path
                                // as a user `send`), preserving cancellation,
                                // approval, tool use, model selection, compaction,
                                // and telemetry.
                                let rendered = match plugins::render_prompt_command(
                                    &cfg,
                                    &args,
                                    &ws,
                                    &session_id,
                                ) {
                                    Ok(p) => p,
                                    Err(e) => {
                                        emit(&Event::new("error").with("message", json!(e)));
                                        continue;
                                    }
                                };
                                if rendered.trim().is_empty() {
                                    emit(&Event::new("error").with(
                                        "message",
                                        json!(format!(
                                            "/{name} requires a request. Usage: /{name} <research request>"
                                        )),
                                    ));
                                    continue;
                                }
                                // Resolve the model + effort the same way a bare
                                // `send` would: last-used model (if still known),
                                // else the configured default, else the first
                                // available model. Effort defaults to "medium"
                                // (matching `send`/`steer`).
                                let models = state.models.read().await.clone();
                                let last_model = state.last_model.lock().await.clone();
                                let default_model = state.cfg.read().await.default_model.clone();
                                let model = last_model
                                    .filter(|m| models.iter().any(|mi| &mi.id == m))
                                    .or_else(|| default_model.clone())
                                    .or_else(|| models.first().map(|m| m.id.clone()))
                                    .unwrap_or_default();
                                if model.is_empty() {
                                    emit(&Event::new("error").with(
                                        "message",
                                        json!("no model available; set a provider/key first"),
                                    ));
                                    continue;
                                }
                                // Surface the original command (not the full
                                // rendered prompt) so the transcript stays
                                // readable while the model receives the protocol.
                                let shown = if args.trim().is_empty() {
                                    format!("/{name}")
                                } else {
                                    format!("/{name} {args}")
                                };
                                emit(&Event::new("info").with(
                                    "message",
                                    json!(format!("{shown} — starting agent turn")),
                                ));
                                let st = state.clone();
                                let client_c = client.clone();
                                let effort = "medium".to_string();
                                start_turn(&st, &client_c, model, None, rendered, effort, None)
                                    .await;
                            }
                        }
                    }
                    None => {
                        emit(
                            &Event::new("error")
                                .with("message", json!(format!("unknown plugin command '{name}'"))),
                        );
                    }
                }
            }
            Command::GetVisionConfig => {
                let vc = state.vision.read().await.clone();
                let models = state.models.read().await.clone();
                let models_json: Vec<Value> = models
                    .iter()
                    .map(|m| {
                        json!({
                            "id": m.id.clone(),
                            "vision": m.vision || vc.has_vision(m.id.as_str()),
                            "provider": m.provider.clone(),
                            "cost_rank": vision::vision_cost_rank(&m.id),
                        })
                    })
                    .collect();
                emit(
                    &Event::new("vision_config")
                        .with("enabled", json!(vc.enabled))
                        .with("vision_models", json!(vc.vision_models.clone()))
                        .with("vision_model", json!(vc.vision_model.clone()))
                        .with("models", json!(models_json)),
                );
            }
            Command::SetVisionConfig {
                enabled,
                vision_models,
                vision_model,
            } => {
                let vc = VisionConfig {
                    enabled,
                    vision_models,
                    vision_model: vision_model.filter(|s| !s.is_empty()),
                };
                let workspace = state.cfg.read().await.workspace.clone();
                vc.save(&workspace);
                *state.vision.write().await = vc.clone();
                let models = state.models.read().await.clone();
                let models_json: Vec<Value> = models
                    .iter()
                    .map(|m| {
                        json!({
                            "id": m.id.clone(),
                            "vision": m.vision || vc.has_vision(m.id.as_str()),
                            "provider": m.provider.clone(),
                            "cost_rank": vision::vision_cost_rank(&m.id),
                        })
                    })
                    .collect();
                emit(
                    &Event::new("vision_config")
                        .with("enabled", json!(vc.enabled))
                        .with("vision_models", json!(vc.vision_models.clone()))
                        .with("vision_model", json!(vc.vision_model.clone()))
                        .with("models", json!(models_json)),
                );
            }
            Command::ListSkills => {
                // Re-publish the discoverable-skills list (project then user
                // scope). The TUI/web request this after a turn ends so a skill
                // created mid-session (e.g. by /reflect or /index) shows up in
                // the `/skill:<name>` autocomplete without a restart.
                let ws = state.cfg.read().await.workspace.clone();
                emit_skills_event(&ws);
            }
            Command::ListMarketplaceSkills => {
                let ws = state.cfg.read().await.workspace.clone();
                emit(
                    &Event::new("skill_marketplace_state")
                        .with(
                            "disclaimer_accepted",
                            json!(skill_marketplace::disclaimer_accepted()),
                        )
                        .with("installed", json!(skill_marketplace::installed(&ws))),
                );
            }
            Command::AcceptSkillDisclaimer => match skill_marketplace::accept_disclaimer() {
                Ok(()) => {
                    let ws = state.cfg.read().await.workspace.clone();
                    emit(
                        &Event::new("skill_marketplace_state")
                            .with("disclaimer_accepted", json!(true))
                            .with("installed", json!(skill_marketplace::installed(&ws))),
                    );
                }
                Err(error) => {
                    emit(&Event::new("skill_marketplace_error").with("message", json!(error)))
                }
            },
            Command::SearchMarketplaceSkills { query } => {
                match skill_marketplace::search(&client, &query).await {
                    Ok(skills) => emit(
                        &Event::new("skill_marketplace_results")
                            .with("query", json!(query))
                            .with("skills", json!(skills)),
                    ),
                    Err(error) => {
                        emit(&Event::new("skill_marketplace_error").with("message", json!(error)))
                    }
                }
            }
            Command::InstallMarketplaceSkill {
                source,
                name,
                scope,
            } => {
                let ws = state.cfg.read().await.workspace.clone();
                match skill_marketplace::SkillScope::parse(&scope) {
                    Ok(scope) => {
                        match skill_marketplace::install(&client, &ws, &source, &name, scope).await
                        {
                            Ok(_) => {
                                emit(
                                    &Event::new("skill_marketplace_changed")
                                        .with("action", json!("installed"))
                                        .with("name", json!(name))
                                        .with("scope", json!(scope.as_str())),
                                );
                                emit_skills_event(&ws);
                            }
                            Err(error) => emit(
                                &Event::new("skill_marketplace_error")
                                    .with("message", json!(error)),
                            ),
                        }
                    }
                    Err(error) => {
                        emit(&Event::new("skill_marketplace_error").with("message", json!(error)))
                    }
                }
            }
            Command::UpdateMarketplaceSkill { name, scope } => {
                let ws = state.cfg.read().await.workspace.clone();
                match skill_marketplace::SkillScope::parse(&scope) {
                    Ok(scope) => {
                        match skill_marketplace::update(&client, &ws, &name, scope).await {
                            Ok((_, changed)) => {
                                emit(
                                    &Event::new("skill_marketplace_changed")
                                        .with(
                                            "action",
                                            json!(if changed { "updated" } else { "unchanged" }),
                                        )
                                        .with("name", json!(name))
                                        .with("scope", json!(scope.as_str())),
                                );
                                emit_skills_event(&ws);
                            }
                            Err(error) => emit(
                                &Event::new("skill_marketplace_error")
                                    .with("message", json!(error)),
                            ),
                        }
                    }
                    Err(error) => {
                        emit(&Event::new("skill_marketplace_error").with("message", json!(error)))
                    }
                }
            }
            Command::RemoveMarketplaceSkill { name, scope } => {
                let ws = state.cfg.read().await.workspace.clone();
                match skill_marketplace::SkillScope::parse(&scope) {
                    Ok(scope) => match skill_marketplace::remove(&ws, &name, scope) {
                        Ok(()) => {
                            emit(
                                &Event::new("skill_marketplace_changed")
                                    .with("action", json!("removed"))
                                    .with("name", json!(name))
                                    .with("scope", json!(scope.as_str())),
                            );
                            emit_skills_event(&ws);
                        }
                        Err(error) => emit(
                            &Event::new("skill_marketplace_error").with("message", json!(error)),
                        ),
                    },
                    Err(error) => {
                        emit(&Event::new("skill_marketplace_error").with("message", json!(error)))
                    }
                }
            }
            Command::RefreshModels => {
                // On-demand model-cache refresh (TUI `/refresh`, web refresh
                // button). Force a live discovery for every logged-in provider
                // (bypassing the 8h disk-cache TTL), then re-aggregate + re-emit.
                //
                // IMPORTANT: run off the command loop. Live /models + models.dev
                // enrichment can take many seconds on a dead/slow host; awaiting
                // here would freeze every other command (same gotcha as
                // `discover_provider_models`). Emit an info event up front so
                // clients can show a spinner until `models_refreshed` arrives.
                emit(&Event::new("info").with(
                    "message",
                    json!("refreshing model list (live fetch from all logged-in providers)…"),
                ));
                let st = state.clone();
                let cl = client.clone();
                state
                    .logger
                    .log("refresh_models", json!({ "phase": "start" }));
                tokio::spawn(async move {
                    st.refresh_models_force(&cl).await;
                    st.logger.log("refresh_models", json!({ "phase": "done" }));
                });
            }
            Command::ListAgents => {
                let cfg = state.cfg.read().await;
                emit_agents_event(&cfg.workspace, &cfg);
            }
            Command::ApplySkill {
                name,
                task,
                model,
                reasoning_effort,
            } => {
                let st = state.clone();
                let client = client.clone();
                let models = st.models.read().await.clone();
                if !models.iter().any(|m| m.id == model) {
                    emit_turn_rejected(format!("unknown model: {model}"));
                    continue;
                }
                let ws = st.cfg.read().await.workspace.clone();
                let skills = subagent::discover_skills_full(&ws);
                let skill = skills
                    .into_iter()
                    .find(|s| s.name.eq_ignore_ascii_case(&name));
                let Some(skill) = skill else {
                    emit_turn_rejected(format!(
                        "unknown skill '{name}' — use /skill:<name> with a name from the autocomplete"
                    ));
                    continue;
                };
                let effort = reasoning_effort.unwrap_or_else(|| "medium".into());
                let prompt = build_skill_prompt(&skill, task.as_deref());
                start_turn(&st, &client, model, None, prompt, effort, None).await;
            }
            Command::StartGoal {
                goal: goal_text,
                concurrency,
                max_tasks,
                allowed_models,
                allowed_providers,
                auto_deploy,
                ceo_mode,
                max_iterations,
                max_plan_revisions,
                planner_model,
                worker_model,
                reviewer_model,
                model_concurrency,
                model,
                reasoning_effort,
            } => {
                let models = state.models.read().await.clone();
                if !models.iter().any(|m| m.id == model) {
                    emit_turn_rejected(format!("unknown model: {model}"));
                    continue;
                }
                // Cancel any prior goal deploy.
                cancel_goal_deploy(&state).await;
                let cfg = state.cfg.read().await;
                let defaults = (
                    cfg.subagents.parallel_concurrency,
                    cfg.subagents.parallel_max_tasks,
                );
                drop(cfg);
                match goal::new_goal(goal::StartGoalArgs {
                    goal: goal_text,
                    concurrency,
                    max_tasks,
                    allowed_models: allowed_models.unwrap_or_default(),
                    allowed_providers: allowed_providers.unwrap_or_default(),
                    auto_deploy,
                    ceo_mode,
                    max_iterations,
                    max_plan_revisions,
                    role_models: goal::RoleModels {
                        planner: planner_model,
                        worker: worker_model,
                        reviewer: reviewer_model,
                    },
                    model_concurrency: model_concurrency.unwrap_or_default(),
                    model: model.clone(),
                    reasoning_effort: reasoning_effort.clone(),
                    default_concurrency: defaults.0,
                    default_max_tasks: defaults.1,
                }) {
                    Ok(mode) => {
                        let prompt = goal::planning_prompt(&mode);
                        let effort = mode.reasoning_effort.clone();
                        // Planning turn uses parent_model (may be planner role pin).
                        let plan_model = mode.parent_model.clone();
                        // Seed WorkState.goal (replace, not one-shot).
                        {
                            let mut ws = state.work_state.lock().await;
                            ws.goal = truncate_str(&mode.goal, 240);
                            ws.done.clear();
                            ws.in_progress.clear();
                            ws.next.clear();
                            ws.last_activity = "goal:planning".into();
                            ws.touch();
                        }
                        emit_work_state(&state).await;
                        {
                            let mut g = state.goal.lock().await;
                            *g = mode;
                            goal::emit_goal_state(&g);
                        }
                        emit(&Event::new("info").with("message", json!("Goal mode: planning…")));
                        // Prefer planner role model when set; else selected model.
                        let turn_model = if models.iter().any(|m| m.id == plan_model) {
                            plan_model
                        } else {
                            model
                        };
                        // Speculative scout while the planner runs (readonly recon).
                        {
                            let st_scout = state.clone();
                            let client_scout = client.clone();
                            let (goal_for_scout, goal_id_for_scout) = {
                                let g = state.goal.lock().await;
                                (g.goal.clone(), g.id.clone())
                            };
                            let parent = turn_model.clone();
                            let session = state.runtime.session_context();
                            let Some(resource) = state.runtime.register_session_resource(
                                &session,
                                ResourceKind::Subagent,
                                "goal_speculative_scout",
                            ) else {
                                emit(&Event::new("error").with(
                                    "message",
                                    json!(
                                        "goal scout was not started because its session is stale"
                                    ),
                                ));
                                continue;
                            };
                            let scout_cancel = resource.cancellation().clone();
                            *state.goal_deploy_cancel.lock().await = Some(scout_cancel.clone());
                            tokio::spawn(runtime::scope_session(session.clone(), async move {
                                let provider = st_scout.resolve_provider_for_model(&parent).await;
                                let args = json!({
                                    "agent": "scout",
                                    "task": format!(
                                        "Quick readonly reconnaissance for this goal (do not modify files):\n{goal_for_scout}\n\nSummarize relevant files, risks, and entry points in under 800 words."
                                    ),
                                    "context": "fresh",
                                    "_parent_run_id": goal_id_for_scout,
                                });
                                let outcome = crate::subagent::execute(
                                    st_scout.clone(),
                                    client_scout,
                                    provider,
                                    parent,
                                    args,
                                    scout_cancel.clone(),
                                    0,
                                )
                                .await;
                                if outcome.ok
                                    && !scout_cancel.is_cancelled()
                                    && st_scout.runtime.is_session_active(&session)
                                    && !outcome.output.trim().is_empty()
                                {
                                    let mut g = st_scout.goal.lock().await;
                                    let text: String = outcome.output.chars().take(4000).collect();
                                    g.scout_findings = Some(text);
                                    emit(&Event::new("info").with(
                                        "message",
                                        json!("speculative scout finished — findings available for deploy"),
                                    ));
                                }
                                drop(resource);
                            }));
                        }
                        start_turn(&state, &client, turn_model, None, prompt, effort, None).await;
                    }
                    Err(e) => {
                        emit(&Event::new("error").with("message", json!(e)));
                    }
                }
            }
            Command::CancelGoal => {
                cancel_goal_deploy(&state).await;
                state
                    .goal_wrapup_active
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                // Goal cancellation is a lifecycle cancellation, not merely a
                // phase update: stop the planner, all owned subagents/workers,
                // approvals, subprocesses, and intercom through the same path
                // used by abort and session replacement.
                cancel_in_flight_turn(&state, CancellationReason::GoalCancelled, false).await;
                let mut g = state.goal.lock().await;
                if !g.is_active() {
                    emit(&Event::new("info").with("message", json!("no active goal to cancel")));
                } else {
                    goal::cancel_goal(&mut g, "cancelled by user");
                    emit(&Event::new("info").with("message", json!("goal cancelled")));
                }
            }
            Command::GoalStatus => {
                let g = state.goal.lock().await;
                goal::emit_goal_state(&g);
                goal::emit_goal_plan(&g);
            }
            Command::ApproveGoalPlan => {
                let (should_deploy, model) = {
                    let mut g = state.goal.lock().await;
                    if g.phase != goal::GoalPhase::PlanReady {
                        emit(&Event::new("error").with(
                            "message",
                            json!(format!(
                                "approve_goal_plan requires phase plan_ready (got {})",
                                g.phase.as_str()
                            )),
                        ));
                        (false, String::new())
                    } else if g.prompts.is_empty() {
                        emit(
                            &Event::new("error")
                                .with("message", json!("no plan prompts to deploy")),
                        );
                        (false, String::new())
                    } else {
                        g.deploy_after_turn = false;
                        g.auto_deploy = true;
                        // Close the approve→checkpoint dark gap: emit state +
                        // lasting bridge before the (possibly slow) snapshot.
                        g.touch();
                        goal::emit_goal_state(&g);
                        emit(&Event::new("info").with(
                            "message",
                            json!("Goal plan approved — deploying (snapshotting workspace…)"),
                        ));
                        (true, g.parent_model.clone())
                    }
                };
                if should_deploy {
                    let _ = model;
                    spawn_goal_deploy(state.clone(), client.clone());
                }
            }
            Command::ReviseGoal {
                feedback,
                model,
                reasoning_effort,
            } => {
                let models = state.models.read().await.clone();
                if !models.iter().any(|m| m.id == model) {
                    emit_turn_rejected(format!("unknown model: {model}"));
                    continue;
                }
                cancel_goal_deploy(&state).await;
                let prompt = {
                    let mut g = state.goal.lock().await;
                    if g.goal.is_empty() {
                        emit_turn_rejected("no goal to revise — use start_goal first");
                        None
                    } else {
                        g.revise_feedback = Some(feedback);
                        g.plan = None;
                        g.prompts.clear();
                        g.error = None;
                        g.deploy_after_turn = false;
                        g.parent_model = model.clone();
                        if let Some(e) = reasoning_effort {
                            g.reasoning_effort = e;
                        }
                        goal::transition(&mut g, goal::GoalPhase::Planning, Some("revising plan"));
                        Some((goal::planning_prompt(&g), g.reasoning_effort.clone()))
                    }
                };
                if let Some((prompt, effort)) = prompt {
                    start_turn(&state, &client, model, None, prompt, effort, None).await;
                }
            }
            Command::UserBash {
                command,
                exclude_from_context,
            } => {
                // Off the command loop: sudo waits on `sudo_reply`, which is
                // delivered on this same loop — awaiting here deadlocks.
                let st = state.clone();
                tokio::spawn(async move {
                    handle_user_bash(&st, command, exclude_from_context).await;
                });
            }
            Command::RefreshMemory => {
                let msg = refresh_memory_injection(&state).await;
                emit(&Event::new("info").with("message", json!(msg)));
            }
            Command::SaveMemory { text, tags, scope } => {
                if text.trim().is_empty() {
                    emit(
                        &Event::new("error")
                            .with("message", json!("save_memory: 'text' must not be empty")),
                    );
                } else {
                    // Derive a name from the text (first words + timestamp) so
                    // the slug/filename is unique and human-readable.
                    let name = {
                        let stem: String = text
                            .split_whitespace()
                            .take(5)
                            .collect::<Vec<_>>()
                            .join(" ");
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        format!("{stem} [{ts}]")
                    };
                    let mem_type = tags
                        .as_ref()
                        .and_then(|t| t.first().cloned())
                        .unwrap_or_else(|| "note".to_string());
                    let ws = state.cfg.read().await.workspace.clone();
                    let mem_scope = memory::Scope::parse(scope.as_deref().unwrap_or("workspace"));
                    let save_result = if let Some(mp) = state.plugin_manager.memory_provider() {
                        let args = json!({
                            "name": name,
                            "content": text,
                            "type": mem_type,
                            "description": "",
                            "scope": mem_scope.as_str(),
                        });
                        let r = plugins::execute_memory_provider(
                            &mp,
                            "save",
                            &args,
                            &ws.display().to_string(),
                            "",
                        )
                        .await;
                        if r.ok {
                            Ok(r.id)
                        } else {
                            Err(r.output)
                        }
                    } else {
                        memory::save_memory_scoped(&ws, mem_scope, &name, &text, &mem_type, "").map(
                            |p| {
                                p.file_stem()
                                    .map(|s| s.to_string_lossy().into_owned())
                                    .unwrap_or_default()
                            },
                        )
                    };
                    match save_result {
                        Ok(id) => {
                            // Refresh the injection so the next turn sees the new memory.
                            let _ = refresh_memory_injection(&state).await;
                            emit(
                                &Event::new("memory_saved")
                                    .with("id", json!(id))
                                    .with("message", json!("memory saved")),
                            );
                        }
                        Err(e) => {
                            emit(
                                &Event::new("error")
                                    .with("message", json!(format!("save_memory failed: {e}"))),
                            );
                        }
                    }
                }
            }
            Command::ListMemory => {
                let ws = state.cfg.read().await.workspace.clone();
                let arr: Vec<Value> = if let Some(mp) = state.plugin_manager.memory_provider() {
                    let r = plugins::execute_memory_provider(
                        &mp,
                        "list",
                        &json!({}),
                        &ws.display().to_string(),
                        "",
                    )
                    .await;
                    if r.ok && !r.entries.is_empty() {
                        r.entries
                    } else if r.ok {
                        Vec::new()
                    } else {
                        emit(&Event::new("error").with(
                            "message",
                            json!(format!("list_memory failed: {}", r.output)),
                        ));
                        Vec::new()
                    }
                } else {
                    let entries = memory::scan_all_memories(&ws);
                    entries
                        .iter()
                        .map(|m| {
                            let id = m
                                .path
                                .file_stem()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            json!({
                                "id": id,
                                "name": m.name,
                                "type": m.mem_type,
                                "description": m.description,
                                "content": m.content,
                                "scope": m.scope.as_str(),
                                // Display fields consumed by the TUI's /memory list:
                                // `text` is the scannable label (the memory name),
                                // `tags` surfaces the type as a single tag.
                                "text": m.name,
                                "tags": [m.mem_type],
                            })
                        })
                        .collect()
                };
                emit(
                    &Event::new("memory_list")
                        .with("entries", json!(arr))
                        .with("count", json!(arr.len())),
                );
            }
            Command::ForgetMemory { id, scope } => {
                let ws = state.cfg.read().await.workspace.clone();
                let result = if let Some(mp) = state.plugin_manager.memory_provider() {
                    let mut args = json!({ "id": id });
                    if let Some(s) = scope.as_deref().filter(|s| !s.is_empty()) {
                        args["scope"] = json!(s);
                    }
                    let r = plugins::execute_memory_provider(
                        &mp,
                        "forget",
                        &args,
                        &ws.display().to_string(),
                        "",
                    )
                    .await;
                    if r.ok {
                        Ok(())
                    } else {
                        Err(r.output)
                    }
                } else {
                    match scope.as_deref() {
                        Some(s) if !s.is_empty() => {
                            memory::forget_memory_scoped(&ws, memory::Scope::parse(s), &id)
                        }
                        _ => memory::forget_memory_any(&ws, &id),
                    }
                };
                match result {
                    Ok(()) => {
                        let _ = refresh_memory_injection(&state).await;
                        emit(
                            &Event::new("memory_saved")
                                .with("message", json!(format!("forgot memory '{id}'"))),
                        );
                    }
                    Err(e) => {
                        emit(
                            &Event::new("error")
                                .with("message", json!(format!("forget_memory failed: {e}"))),
                        );
                    }
                }
            }
            Command::Approve {
                request_id,
                decision,
                pattern,
            } => {
                // Look up by the unique approval id (the request_id the TUI
                // echoes back), not the tool-call id — concurrent approvals from
                // parallel subagents (which may each use `call_1`) can't resolve
                // to the wrong request.
                let p = state.pending.lock().await.get(&request_id).cloned();
                if let Some(p) = p {
                    let active = state.runtime.snapshot();
                    let identity_matches = active.session_id.as_str() == p.session_id
                        && !p.cancellation.is_cancelled()
                        && (!p.coordinator_bound
                            || active.run_id.as_ref().map(|id| id.as_str())
                                == Some(p.run_id.as_str()));
                    if !identity_matches {
                        state.pending.lock().await.remove(&request_id);
                        emit(&Event::new("error").with(
                            "message",
                            json!(format!(
                                "approval {request_id} is stale and cannot authorize a tool call"
                            )),
                        ));
                        continue;
                    }
                    match decision.as_str() {
                        "yes" => *p.granted.lock().await = Some(true),
                        "always" => {
                            *p.granted.lock().await = Some(true);
                            *p.escalated.lock().await = true;
                        }
                        "allow_session" => {
                            *p.granted.lock().await = Some(true);
                            *p.allow_session.lock().await = true;
                        }
                        "allow_pattern" => {
                            let pat = pattern.or_else(|| {
                                p.args
                                    .get("path")
                                    .and_then(|v| v.as_str())
                                    .map(String::from)
                                    .or_else(|| {
                                        p.args
                                            .get("command")
                                            .and_then(|v| v.as_str())
                                            .map(String::from)
                                    })
                            });
                            if pat.as_deref().is_some_and(|candidate| {
                                approval_pattern_within_requested_scope(&p.args, candidate)
                            }) {
                                *p.granted.lock().await = Some(true);
                                *p.allow_pattern.lock().await = pat;
                            } else {
                                *p.granted.lock().await = Some(false);
                                emit(&Event::new("error").with(
                                    "message",
                                    json!("approval pattern exceeds or does not match the requested scope"),
                                ));
                            }
                        }
                        _ => *p.granted.lock().await = Some(false),
                    }
                    p.notify.notify_one();
                } else {
                    emit(&Event::new("error").with(
                        "message",
                        json!(format!(
                            "unknown, expired, or already resolved approval request: {request_id}"
                        )),
                    ));
                }
            }
            Command::IntercomReply { request_id, reply } => {
                // The orchestrator (user, via the TUI) replies to a subagent's
                // contact_supervisor need_decision ask. Resolves the pending ask
                // so the awaiting subagent loop wakes and continues.
                let ok = state.intercom.resolve_ask(&request_id, &reply);
                if ok {
                    emit(&Event::new("info").with("message", json!("reply delivered to subagent")));
                } else {
                    emit(&Event::new("error").with(
                        "message",
                        json!(format!("no pending intercom ask for id {request_id}")),
                    ));
                }
            }
            Command::JobList => {
                let mut runs: std::collections::BTreeMap<String, Value> = state
                    .subagent_runs
                    .lock()
                    .await
                    .values()
                    .map(|run| {
                        (
                            run.id.clone(),
                            json!({
                                "run_id": run.id,
                                "parent_run_id": run.parent_run_id,
                                "state": run.state,
                                "started_at": run.started_at,
                                "ended_at": run.ended_at,
                                "summary": run.summary,
                            }),
                        )
                    })
                    .collect();
                if let Some(path) = state.cfg.read().await.session_file.clone() {
                    if let Ok(report) = session::load_report(&path) {
                        for delivery in report.parent_deliveries {
                            if let Some(artifact) =
                                session::read_job_artifact(&path, &delivery.run_id)
                            {
                                runs.entry(delivery.run_id.clone()).or_insert(artifact);
                            }
                        }
                    }
                    if let Ok(entries) = std::fs::read_dir(
                        path.parent()
                            .unwrap_or_else(|| std::path::Path::new("."))
                            .join("artifacts"),
                    ) {
                        for entry in entries.flatten() {
                            if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                                continue;
                            }
                            if let Ok(raw) = std::fs::read_to_string(entry.path()) {
                                if let Ok(artifact) = serde_json::from_str::<Value>(&raw) {
                                    if let Some(id) = artifact.get("run_id").and_then(Value::as_str)
                                    {
                                        runs.entry(id.to_string()).or_insert(artifact);
                                    }
                                }
                            }
                        }
                    }
                }
                emit(
                    &Event::new("job_list")
                        .with("runs", json!(runs.into_values().collect::<Vec<_>>())),
                );
            }
            Command::JobStatus { run_id } => {
                let run = state.subagent_runs.lock().await.get(&run_id).cloned();
                if let Some(run) = run {
                    emit(
                        &Event::new("job_status")
                            .with("run_id", json!(run.id))
                            .with("parent_run_id", json!(run.parent_run_id))
                            .with("state", json!(run.state))
                            .with("started_at", json!(run.started_at))
                            .with("ended_at", json!(run.ended_at))
                            .with("summary", json!(run.summary)),
                    );
                } else if let Some(path) = state.cfg.read().await.session_file.clone() {
                    if let Some(artifact) = session::read_job_artifact(&path, &run_id) {
                        emit(
                            &Event::new("job_status")
                                .with("run_id", json!(run_id))
                                .with("artifact", artifact),
                        );
                    } else {
                        emit(
                            &Event::new("error")
                                .with("message", json!(format!("unknown job {run_id}"))),
                        );
                    }
                } else {
                    emit(
                        &Event::new("error")
                            .with("message", json!(format!("unknown job {run_id}"))),
                    );
                }
            }
            Command::JobCancel { run_id } => {
                let cancel = state
                    .subagent_runs
                    .lock()
                    .await
                    .get(&run_id)
                    .and_then(|run| run.cancel.clone());
                if let Some(cancel) = cancel {
                    cancel.cancel();
                    emit(&Event::new("job_cancel_requested").with("run_id", json!(run_id)));
                } else if let Some(path) = state.cfg.read().await.session_file.clone() {
                    if let Some(artifact) = session::read_job_artifact(&path, &run_id) {
                        emit(
                            &Event::new("job_cancel_result")
                                .with("run_id", json!(run_id))
                                .with("artifact", artifact),
                        );
                    } else {
                        emit(
                            &Event::new("error")
                                .with("message", json!(format!("job {run_id} is not live"))),
                        );
                    }
                } else {
                    emit(
                        &Event::new("error")
                            .with("message", json!(format!("job {run_id} is not live"))),
                    );
                }
            }
            Command::JobWait { run_id, timeout_ms } => {
                // Off the command loop so abort/approve/send keep flowing while
                // we poll. Cap raised to 10m; still clamp absurd values.
                let st = state.clone();
                let timeout = timeout_ms.unwrap_or(30_000).min(600_000);
                tokio::spawn(async move {
                    let deadline =
                        std::time::Instant::now() + std::time::Duration::from_millis(timeout);
                    loop {
                        let run = st.subagent_runs.lock().await.get(&run_id).cloned();
                        if let Some(run) = run {
                            if run.state != "running" && run.state != "paused" {
                                emit(
                                    &Event::new("job_wait_result")
                                        .with("run_id", json!(run.id))
                                        .with("state", json!(run.state))
                                        .with("summary", json!(run.summary)),
                                );
                                break;
                            }
                        } else if let Some(path) = st.cfg.read().await.session_file.clone() {
                            if let Some(artifact) = session::read_job_artifact(&path, &run_id) {
                                emit(
                                    &Event::new("job_wait_result")
                                        .with("run_id", json!(run_id))
                                        .with("artifact", artifact),
                                );
                                break;
                            }
                        }
                        if std::time::Instant::now() >= deadline {
                            emit(&Event::new("job_wait_timeout").with("run_id", json!(run_id)));
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    }
                });
            }
            Command::SessionTree => {
                if let Some(path) = state.cfg.read().await.session_file.clone() {
                    emit(&Event::new("session_tree").with("tree", session::session_tree(&path)));
                } else {
                    emit(&Event::new("error").with("message", json!("no active session")));
                }
            }
            Command::SessionBranch { entry_id } => {
                if let Some(path) = state.cfg.read().await.session_file.clone() {
                    match session::create_branch(&path, &entry_id) {
                        Ok(branch_id) => {
                            let loaded = session::load(&path).unwrap_or_default();
                            *state.conversation.lock().await = loaded.clone();
                            let est = estimate_messages_tokens(&loaded);
                            *state.estimated_tokens.lock().await = est;
                            state.invalidate_real_token_baseline().await;
                            let visible: Vec<Value> = loaded
                                .iter()
                                .filter(|m| !m.is_system())
                                .map(Value::from)
                                .collect();
                            emit(
                                &Event::new("history")
                                    .with("messages", json!(visible))
                                    .with("tokens_in", json!(est)),
                            );
                            emit(
                                &Event::new("session_branch")
                                    .with("entry_id", json!(branch_id))
                                    .with("parent_id", json!(entry_id)),
                            );
                        }
                        Err(error) => emit(&Event::new("error").with("message", json!(error))),
                    }
                } else {
                    emit(&Event::new("error").with("message", json!("no active session")));
                }
            }
            Command::AskReply {
                request_id,
                answers,
            } => {
                // The user answered (or skipped) a pending `ask` tool call.
                // Resolves the awaiting request_ask() so the model continues.
                let p = state.pending_asks.lock().await.get(&request_id).cloned();
                if let Some(p) = p {
                    *p.answers.lock().await = Some(answers);
                    p.notify.notify_one();
                } else {
                    emit(&Event::new("error").with(
                        "message",
                        json!(format!("no pending ask for id {request_id}")),
                    ));
                }
            }
            Command::SudoReply {
                request_id,
                approved,
                password,
            } => {
                // The user approved (with password) or declined (Esc) a pending
                // sudo_request. Resolves the awaiting request_sudo() so the
                // blocked bash call either runs with `sudo -S` or returns a
                // "declined" outcome to the agent.
                let p = state.pending_sudos.lock().await.get(&request_id).cloned();
                if let Some(p) = p {
                    *p.result.lock().await = Some(if approved { password } else { None });
                    p.notify.notify_one();
                } else {
                    emit(&Event::new("error").with(
                        "message",
                        json!(format!("no pending sudo request for id {request_id}")),
                    ));
                }
            }
            Command::Abort => {
                // Cancel the running turn AND drop any queued follow-up/steer so a
                // single abort fully stops the loop (not just the current turn).
                // If a goal/mission is active, also cancel deploy + fail the goal
                // (Control Center Abort sends cancel_goal; bare Abort must match).
                cancel_goal_deploy(&state).await;
                state
                    .goal_wrapup_active
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                {
                    let mut g = state.goal.lock().await;
                    if g.is_active() {
                        goal::cancel_goal(&mut g, "cancelled by user");
                        emit(&Event::new("info").with("message", json!("goal cancelled")));
                    }
                }
                cancel_in_flight_turn(&state, CancellationReason::Abort, false).await;
                // Always pair aborted+done so clients that gate "working" on
                // `done` do not stick after Esc on an already-idle core.
                emit(&Event::new("aborted"));
                emit(&Event::new("done"));
            }
            Command::ClearQueue => {
                // Drop a queued follow-up/steer but leave the running turn alone —
                // the TUI's Esc uses this to cancel just the queued message.
                // (If a steer already cancelled the in-flight turn, that turn's
                // `aborted` will still fire; clearing here means the steer won't
                // run and the loop winds down to idle.)
                let had = state.queued.lock().await.take().is_some();
                emit(&Event::new("info").with(
                    "message",
                    json!(if had {
                        "queue cleared"
                    } else {
                        "queue already empty"
                    }),
                ));
            }
            Command::Send {
                prompt,
                model,
                provider,
                reasoning_effort,
                images,
            } => {
                let st = state.clone();
                let client = client.clone();
                let models = st.models.read().await.clone();
                let valid = models.iter().any(|m| m.id == model);
                if !valid {
                    emit_turn_rejected(format!("unknown model: {model}"));
                    continue;
                }
                let effort = reasoning_effort.unwrap_or_else(|| "medium".into());
                // If a turn is already running, buffer this prompt (one-deep) instead
                // of dropping it. It drains when the running turn emits `done`.
                start_turn(&st, &client, model, provider, prompt, effort, images).await;
            }
            Command::Steer {
                prompt,
                model,
                provider,
                reasoning_effort,
            } => {
                let st = state.clone();
                let client_c = client.clone();
                let models = st.models.read().await.clone();
                if !models.iter().any(|m| m.id == model) {
                    emit_turn_rejected(format!("unknown model: {model}"));
                    continue;
                }
                let effort = reasoning_effort.unwrap_or_else(|| "medium".into());
                // Steer = interrupt the running turn and redirect it. Cancel the
                // in-flight token and set the steer as the next queued prompt
                // (superseding any queued follow-up); the run_turn drain then runs
                // it, so the `current` token hand-off stays clean. With nothing
                // in flight, steer degrades to a normal turn.
                emit(&Event::new("steer").with("prompt", json!(prompt)));
                if st.current.lock().await.is_some() {
                    *st.queued.lock().await = Some(QueuedPrompt {
                        prompt,
                        model,
                        provider,
                        effort,
                    });
                    if let Some(cancelled) = st.runtime.cancel_current(CancellationReason::Steering)
                    {
                        emit(
                            &Event::new("run_cancelled")
                                .with("session_id", json!(cancelled.session_id))
                                .with("run_id", json!(cancelled.run_id))
                                .with("reason", json!(cancelled.reason.as_str())),
                        );
                        // Legacy queued-turn state machines use `aborted` as the
                        // hand-off signal before the redirected run starts.
                        emit(&Event::new("aborted"));
                    }
                    if let Some(run) = st.current.lock().await.take() {
                        run.cancellation().cancel();
                    }
                } else {
                    let run = st.runtime.start_run();
                    *st.current.lock().await = Some(run.clone());
                    let handle = tokio::spawn(run_turn_and_drain(
                        st.clone(),
                        client_c,
                        model,
                        provider,
                        prompt,
                        effort,
                        None,
                        run,
                    ));
                    *st.handle.lock().await = Some(handle);
                }
            }
        }
    }
    // stdin EOF is process shutdown: invalidate and bound cleanup through the
    // same lifecycle path as interactive cancellation.
    cancel_in_flight_turn(&state, CancellationReason::Shutdown, false).await;
    // P0-H3: true process/session teardown (once). Distinct from per-turn
    // `session_stop` used by telemetry plugins.
    dispatch_lifecycle(&state, "session_shutdown").await;
    // Stop the active sandbox (microVM) cleanly so no guest process outlives the
    // core. Best-effort: a hard kill already reaps the msb runtime subprocess.
    crate::sandbox::shutdown().await;
    // Clean up our presence record so peers don't see a stale session. Best
    // effort — a kill -9 / crash leaves a stale file that `read_peers` reaps
    // by mtime, so this is an optimization (instant disappearance), not a
    // correctness requirement.
    {
        let ws = state.cfg.read().await.workspace.clone();
        presence::clear_presence(&ws, std::process::id());
    }
}
