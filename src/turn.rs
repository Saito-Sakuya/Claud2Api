use crate::{
    engine::{Engine, Output, Reservation},
    error::{Fault, Result},
    native_stream::{WebBlockDecoder, WebEmission},
    protocol::{self, Block, Input, ToolDecoder, Turn},
    store::{Checkpoint, Usage, nonce, now},
    transport::CompletionRequest,
};
use serde_json::json;
use tokio::sync::mpsc;

pub(crate) struct AttemptPlan {
    pub fresh: bool,
    pub checkpoint: Option<Checkpoint>,
    pub compacted: bool,
}
impl Engine {
    async fn projection(
        &self,
        r: &mut Reservation,
        plan: &mut AttemptPlan,
        tx: &mpsc::Sender<Output>,
    ) -> Result<(Input, usize)> {
        let settings = r.settings.clone();
        let mut projected = r.input.clone();
        let mut base = 0;
        if !plan.fresh {
            projected.turns = projected.turns[r.session.history.len()..].to_vec();
            base = r.session.context_estimate;
            if base == 0 {
                let mut prior = r.input.clone();
                prior.turns.truncate(r.session.history.len());
                base = protocol::physical(&prior.prompt(&r.session.nonce));
                prior.turns.clear();
                base += r.session.history.len().div_ceil(2).saturating_sub(1)
                    * protocol::physical(&prior.prompt(&r.session.nonce));
            }
            let reserve = r
                .input
                .max_tokens
                .max(1024)
                .min(settings.context_budget / 4);
            let projected_units =
                base + protocol::physical(&projected.prompt(&r.session.nonce)) + reserve;
            let cut = crate::engine::compaction_cut(&r.input.turns, settings.keep_recent_rounds);
            if settings.compaction_enabled
                && !plan.compacted
                && now() >= r.session.compaction_retry_after
                && cut > 0
                && projected_units
                    >= settings.context_budget * settings.compaction_trigger_percent / 100
            {
                plan.compacted = true;
                Self::send(&r.scope, tx, Output::Ping).await?;
                match self.summarize(r, cut, plan.checkpoint.as_ref()).await {
                    Ok(checkpoint) => {
                        let mut smaller = r.input.clone();
                        smaller.summary = checkpoint.summary.clone();
                        smaller.turns = smaller.turns[checkpoint.covered..].to_vec();
                        let size = protocol::physical(&smaller.prompt(&nonce())) + reserve;
                        if size < settings.context_budget && size * 10 < projected_units * 9 {
                            self.store
                                .mutate(
                                    "context_checkpoint",
                                    json!({"session_id":r.session.id,"covered":checkpoint.covered}),
                                    |state| {
                                        if let Some(session) = state.sessions.get_mut(&r.session.id)
                                        {
                                            session.checkpoint = Some(checkpoint.clone());
                                        }
                                        Ok(())
                                    },
                                )
                                .await?;
                            plan.checkpoint = Some(checkpoint);
                            plan.fresh = true;
                        } else {
                            self.compaction_backoff(&r.session.id).await?;
                        }
                    }
                    Err(fault) if fault.message == "invalid internal handoff summary" => {
                        self.compaction_backoff(&r.session.id).await?
                    }
                    Err(fault) => return Err(fault),
                }
            }
        }
        if plan.fresh {
            projected = r.input.clone();
            if let Some(checkpoint) = &plan.checkpoint {
                projected.summary = checkpoint.summary.clone();
                projected.turns = projected.turns[checkpoint.covered..].to_vec();
            }
            base = 0;
        }
        Ok((projected, base))
    }

    pub(crate) async fn attempt(
        &self,
        r: &mut Reservation,
        tx: &mpsc::Sender<Output>,
        plan: &mut AttemptPlan,
    ) -> Result<()> {
        if let Some(trace) = &r.scope.trace {
            trace.bind_account(&r.lease.account_id);
        }
        if let Some(retry_stop) = r.fresh_retry_stop.take()
            && retry_stop.needs_stop
        {
            if retry_stop.account_id != r.lease.account_id {
                return Err(Fault::busy(
                    "the original account is required to stop the pending response",
                    1,
                ));
            }
            r.lease
                .slot
                .client
                .stop_response(&r.lease, &r.scope, &retry_stop.target)
                .await
                .map_err(|mut fault| {
                    fault.quota = false;
                    fault
                })?;
            self.mark_fresh_retry_stopped(&r.session.id, &r.input)
                .await?;
        }
        let resumed = r.resume.take();
        let resumed = if resumed
            .as_ref()
            .is_some_and(|pending| crate::client_tools::requires_fresh(&r.input, pending))
        {
            // New instructions and tool policies retain their caller roles.
            // PDF results use the supported uploaded-file transport.
            let pending = resumed.expect("fresh replacement requires a pending completion");
            let previous_target = pending.target.clone();
            self.record_fresh_retry_receipt(
                &r.session.id,
                &r.session.active_ticket,
                &r.input,
                r.settings.request_timeout_seconds,
                &previous_target,
                &r.lease.account_id,
            )
            .await?;
            drop(pending);
            r.lease
                .slot
                .client
                .stop_response(&r.lease, &r.scope, &previous_target)
                .await
                .map_err(|mut fault| {
                    fault.quota = false;
                    fault
                })?;
            self.mark_fresh_retry_stopped(&r.session.id, &r.input)
                .await?;
            plan.fresh = true;
            None
        } else {
            resumed
        };
        let resumed_stream = resumed.is_some();
        let client_descriptors = crate::client_tools::descriptors(&r.input);
        let (
            mut completion,
            mut web_decoder,
            target,
            assistant,
            protocol_nonce,
            prompt,
            base,
            started_at,
            mut file_until,
            mut rendered_sources,
        ) = if let Some(pending) = resumed {
            if pending.expires_at <= tokio::time::Instant::now() {
                return Err(Fault::new(
                    409,
                    "invalid_request_error",
                    "client tool continuation expired; start a new conversation",
                ));
            }
            r.lease.check().await?;
            if let Some(trace) = &r.scope.trace {
                trace.begin_usage(protocol::estimate(&r.input.prompt(&pending.protocol_nonce)));
            }
            let (content, is_error) = crate::client_tools::result_content(&r.input, &pending)?;
            r.lease
                .slot
                .client
                .tool_result(
                    &r.lease,
                    &r.scope,
                    &pending.target,
                    &pending.tool_id,
                    content,
                    is_error,
                )
                .await
                .map_err(|mut fault| {
                    fault.quota = false;
                    fault
                })?;
            let payload_estimate =
                protocol::go_json(r.input.turns.last().ok_or_else(Fault::internal)?);
            (
                pending.completion,
                pending.decoder,
                pending.target,
                pending.assistant,
                pending.protocol_nonce,
                payload_estimate,
                r.session.context_estimate,
                pending.started_at,
                pending.file_until,
                pending.rendered_sources,
            )
        } else {
            let models = r
                .lease
                .slot
                .client
                .models(&r.lease, &r.scope, false)
                .await?;
            if !models.contains(&r.input.model) {
                return Err(Fault::invalid(
                    "model is not available in this account's web catalog; use an exact ID from GET /v1/models",
                ));
            }
            let (projected, base) = self.projection(r, plan, tx).await?;
            let protocol_nonce = if plan.fresh {
                nonce()
            } else {
                r.session.nonce.clone()
            };
            let prompt = if r.input.tools.is_empty() {
                projected.prompt(&protocol_nonce)
            } else {
                crate::client_tools::prompt(&projected)
            };
            let uploads = projected.uploads(&projected.turns)?;
            if let Some(trace) = &r.scope.trace {
                trace.begin_usage(protocol::estimate(&r.input.prompt(&protocol_nonce)));
            }
            let target = if plan.fresh {
                let target = r.lease.slot.client.create(&r.lease, &r.scope).await?;
                self.own(&r.lease.account_id, &target).await?;
                if let Some(project) = &r.input.project_uuid {
                    r.lease
                        .slot
                        .client
                        .bind_project(&r.lease, &r.scope, &target, project)
                        .await?;
                }
                target
            } else {
                r.session.conversation_id.clone()
            };
            let parent = if plan.fresh {
                ""
            } else {
                &r.session.parent_uuid
            };
            if plan.compacted {
                Self::send(&r.scope, tx, Output::Ping).await?;
            }
            r.lease.start(&r.scope).await?;
            if !plan.fresh {
                self.store
                    .mutate("", json!({}), |state| {
                        state
                            .sessions
                            .get_mut(&r.session.id)
                            .ok_or_else(Fault::internal)?
                            .uncertain = true;
                        Ok(())
                    })
                    .await?;
            }
            let mut tools = crate::native_tools::descriptors(&r.input.native_tools);
            tools
                .as_array_mut()
                .ok_or_else(Fault::internal)?
                .extend(client_descriptors.clone());
            let started_at = tokio::time::Instant::now();
            let completion = r
                .lease
                .slot
                .client
                .completion(
                    &r.lease,
                    &r.scope,
                    CompletionRequest {
                        target: &target,
                        parent,
                        model: &r.input.model,
                        prompt: &prompt,
                        uploads: &uploads,
                        tools,
                        thinking: "off",
                    },
                )
                .await?;
            let assistant = completion.assistant.clone();
            let names = crate::native_tools::allowed_names(&r.input.native_tools)
                .into_iter()
                .chain(
                    client_descriptors
                        .iter()
                        .filter_map(|tool| tool["name"].as_str().map(str::to_owned)),
                );
            let web_decoder = WebBlockDecoder::new(names);
            (
                completion,
                web_decoder,
                target,
                assistant,
                protocol_nonce,
                prompt,
                base,
                started_at,
                0i64,
                std::collections::HashSet::new(),
            )
        };
        let input_tokens = protocol::estimate(&r.input.prompt(&protocol_nonce));
        if let Some(trace) = &r.scope.trace {
            trace.begin_usage(input_tokens);
        }
        let mut decoder = ToolDecoder::new(&r.input, &protocol_nonce);
        let mut blocks = Vec::new();
        let mut started = false;
        let mut generated = resumed_stream;
        let mut reason = "end_turn".to_owned();
        let mut sequence = None;
        let mut raw_size = 0usize;
        let mut client_active = false;
        let mut waiting_call = None;
        let read = if r.settings.simulated_cache && !plan.fresh && r.session.cache_until > now() {
            r.session.cache_tokens.min(input_tokens)
        } else {
            0
        };
        'stream: loop {
            let event = completion.next(&r.scope).await?;
            let kind = event["type"].as_str().unwrap_or("");
            if kind == "error" || event.get("error").is_some_and(|e| !e.is_null()) {
                let code = crate::transport::error_code(&event);
                let status = crate::transport::error_status(code);
                let mut fault = r.lease.failure(status, None).await?;
                fault.quota &= !generated;
                drop(completion);
                return Err(fault);
            }
            if !matches!(kind, "message_start" | "ping") {
                generated = true;
                self.fresh_retry_generation_started(&r.session.id, &r.session.active_ticket)
                    .await?;
            }
            if matches!(kind, "message_start" | "ping") {
                continue;
            }
            if !started {
                Self::send(
                    &r.scope,
                    tx,
                    Output::Start {
                        input: input_tokens,
                        read,
                    },
                )
                .await?;
                started = true;
            }
            for emission in web_decoder.feed(&event)? {
                match emission {
                    WebEmission::Text(text) => {
                        raw_size = raw_size.saturating_add(text.len());
                        for block in decoder.feed(&text)? {
                            Self::send(&r.scope, tx, Output::Block(block.clone())).await?;
                            protocol::merge_text(&mut blocks, block);
                        }
                    }
                    WebEmission::NativeStart(block) => {
                        client_active = block["name"].as_str().is_some_and(|name| {
                            crate::client_tools::original_name(&r.input, name).is_some()
                        });
                        if client_active {
                            for text in decoder.boundary()? {
                                Self::send(&r.scope, tx, Output::Block(text.clone())).await?;
                                protocol::merge_text(&mut blocks, text);
                            }
                            continue;
                        }
                        if block["type"] == "tool_use"
                            && let Some(trace) = &r.scope.trace
                        {
                            trace.native_start(
                                block["id"].as_str().unwrap_or(""),
                                block["name"].as_str().unwrap_or(""),
                            );
                        }
                        for text in decoder.boundary()? {
                            Self::send(&r.scope, tx, Output::Block(text.clone())).await?;
                            protocol::merge_text(&mut blocks, text);
                        }
                        if r.input.native_extensions {
                            Self::send(
                                &r.scope,
                                tx,
                                Output::NativeStart(crate::native_tools::sanitize(block)),
                            )
                            .await?;
                        }
                    }
                    WebEmission::NativeDelta(delta) => {
                        if client_active {
                            continue;
                        }
                        if r.input.native_extensions {
                            Self::send(
                                &r.scope,
                                tx,
                                Output::NativeDelta(crate::native_tools::sanitize(delta)),
                            )
                            .await?;
                        }
                    }
                    WebEmission::NativeStop(block) => {
                        client_active = false;
                        if let Some(name) = block["name"]
                            .as_str()
                            .and_then(|name| crate::client_tools::original_name(&r.input, name))
                        {
                            if block["type"] == "tool_use" {
                                let call =
                                    decoder.native_call(name.to_owned(), block["input"].clone())?;
                                let Block::Tool { id, .. } = &call else {
                                    return Err(Fault::internal());
                                };
                                waiting_call = Some((
                                    block["id"].as_str().ok_or_else(Fault::upstream)?.to_owned(),
                                    id.clone(),
                                ));
                                raw_size = raw_size.saturating_add(protocol::go_json(&call).len());
                                Self::send(&r.scope, tx, Output::Block(call.clone())).await?;
                                blocks.push(call);
                                break 'stream;
                            }
                            // The web stream echoes client results after the sidechannel POST.
                            // They already belong to the user history, not assistant output.
                            continue;
                        }
                        if block["type"] == "tool_result"
                            && let Some(trace) = &r.scope.trace
                        {
                            trace.native_result(
                                block["tool_use_id"].as_str().unwrap_or(""),
                                block.get("is_error").and_then(serde_json::Value::as_bool),
                            );
                        }
                        let decorated = crate::native_files::decorate(
                            crate::native_tools::sanitize(block),
                            &self.config.gateway_key,
                            &r.session.token_id,
                            &r.lease.account_id,
                            &target,
                            r.settings.session_ttl_seconds.min(3600),
                        )?;
                        file_until = file_until.max(decorated.expires_at.unwrap_or(0));
                        let block = decorated.block;
                        raw_size = raw_size.saturating_add(protocol::go_json(&block).len());
                        if r.input.native_extensions {
                            Self::send(&r.scope, tx, Output::NativeStop(block.clone())).await?;
                            blocks.push(Block::Native { block });
                        } else if let Some(text) = crate::native_output::text(&block) {
                            rendered_sources.extend(crate::native_output::source_urls(&block));
                            let block = Block::text(text);
                            Self::send(&r.scope, tx, Output::Block(block.clone())).await?;
                            protocol::merge_text(&mut blocks, block);
                        }
                    }
                    WebEmission::Citation(citation) => {
                        for text in decoder.boundary()? {
                            Self::send(&r.scope, tx, Output::Block(text.clone())).await?;
                            protocol::merge_text(&mut blocks, text);
                        }
                        let native = json!({"type":"citation","citation":crate::native_tools::sanitize(citation)});
                        raw_size = raw_size.saturating_add(protocol::go_json(&native).len());
                        if r.input.native_extensions {
                            let block = Block::Native { block: native };
                            Self::send(&r.scope, tx, Output::Block(block.clone())).await?;
                            blocks.push(block);
                        } else {
                            let sources = crate::native_output::source_urls(&native);
                            if (sources.is_empty()
                                || sources.iter().any(|url| !rendered_sources.contains(url)))
                                && let Some(text) = crate::native_output::text(&native)
                            {
                                rendered_sources.extend(sources);
                                let block = Block::text(text);
                                Self::send(&r.scope, tx, Output::Block(block.clone())).await?;
                                protocol::merge_text(&mut blocks, block);
                            }
                        }
                    }
                }
            }
            if kind == "message_delta" {
                if let Some(stop) = event
                    .pointer("/delta/stop_reason")
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    reason = stop.to_owned();
                }
                if let Some(stop) = event
                    .pointer("/delta/stop_sequence")
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    sequence = Some(stop.to_owned());
                }
            }
            if kind == "message_stop" {
                break;
            }
        }
        if waiting_call.is_none() {
            web_decoder.finish()?;
        }
        if let Some(trace) = &r.scope.trace {
            trace.native_observation_complete();
        }
        for block in decoder.finish()? {
            Self::send(&r.scope, tx, Output::Block(block.clone())).await?;
            protocol::merge_text(&mut blocks, block);
        }
        if blocks.is_empty() {
            return Err(Fault::new(
                502,
                "api_error",
                "upstream returned no supported content",
            ));
        }
        if !matches!(
            reason.as_str(),
            "end_turn" | "max_tokens" | "stop_sequence" | "refusal"
        ) {
            return Err(Fault::new(
                502,
                "api_error",
                "unsupported upstream stop reason",
            ));
        }
        if blocks.iter().any(|b| matches!(b, Block::Tool { .. })) {
            reason = "tool_use".to_owned();
        }
        let output_tokens = protocol::estimate(&protocol::go_json(&blocks));
        let cache_tokens = if r.settings.simulated_cache {
            input_tokens + output_tokens
        } else {
            0
        };
        let mut full = r.input.clone();
        full.turns.push(Turn {
            role: "assistant".to_owned(),
            content: blocks,
        });
        let hashes = full.hashes(&self.config.gateway_key);
        let units = base + protocol::physical(&prompt) + raw_size.div_ceil(2);
        let event = if plan.fresh && r.session.account_id != r.lease.account_id {
            "session_migrated"
        } else if plan.fresh && plan.compacted {
            "session_compacted"
        } else {
            "turn_committed"
        };
        self.store.mutate(event,json!({"session_id":r.session.id,"account_id":r.lease.account_id,"model":r.input.model}),|state|{
            let old=state.sessions.get(&r.session.id).cloned().ok_or_else(Fault::internal)?;
            if old.conversation_id!=target{state.queue(&old.account_id,&old.conversation_id);}
            state.pending.retain(|j|!(j.account_id==r.lease.account_id&&j.conversation_id==target&&j.next_attempt<=now()));
            if file_until>now(){
                state.queue(&r.lease.account_id,&target);
                if let Some(job)=state.pending.iter_mut().find(|j|j.account_id==r.lease.account_id&&j.conversation_id==target){job.next_attempt=job.next_attempt.max(file_until);}
            }
            let current=state.sessions.get_mut(&r.session.id).ok_or_else(Fault::internal)?;
            current.account_id=r.lease.account_id.clone();
            current.conversation_id=target.clone();
            current.parent_uuid=assistant.clone();
            current.nonce=protocol_nonce.clone();
            current.context_hash=r.input.context(&self.config.gateway_key);
            current.history=hashes;
            current.expires_at=now()+r.settings.session_ttl_seconds as i64;
            current.uncertain=waiting_call.is_some();
            current.active=false;
            current.cache_tokens=cache_tokens;
            current.cache_until=if r.settings.simulated_cache{now()+3600}else{0};
            current.context_estimate=units.min(32<<20);
            current.checkpoint=plan.checkpoint.clone();
            if plan.fresh{current.compaction_retry_after=0;}
            current.model=r.input.model.clone();
            Ok(())
        }).await?;
        self.clear_fresh_retry_receipt(&r.session.id).await;
        if let Some((tool_id, public_id)) = waiting_call {
            let expires_at = (tokio::time::Instant::now()
                + std::time::Duration::from_secs(r.settings.request_timeout_seconds.min(600)))
            .min(started_at + std::time::Duration::from_secs(840));
            self.park_client(
                &r.session.id,
                crate::client_tools::Pending {
                    completion,
                    decoder: web_decoder,
                    target,
                    assistant,
                    protocol_nonce,
                    started_at,
                    expires_at,
                    tool_id,
                    public_id,
                    rendered_sources,
                    file_until,
                    lease: Some(r.lease.take()?),
                    system: crate::client_tools::system_digest(&r.input),
                    system_bytes: r.input.system.len(),
                    request_cancel: r.scope.cancel.clone(),
                    native_tools: r.input.native_tools.clone(),
                    native_extensions: r.input.native_extensions,
                },
            )
            .await?;
        } else {
            drop(completion);
        }
        self.store
            .record_scoped(
                Usage {
                    input_estimate: input_tokens,
                    output_estimate: output_tokens,
                    physical_estimate: units,
                    cache_read_simulated: read,
                    cache_creation_simulated: if r.settings.simulated_cache {
                        input_tokens - read
                    } else {
                        0
                    },
                    status: 200,
                    account_id: r.lease.account_id.clone(),
                    model: r.input.model.clone(),
                    internal: false,
                    ..Usage::default()
                },
                &r.scope,
                r.settings.simulated_cache,
            )
            .await?;
        Self::send(
            &r.scope,
            tx,
            Output::Done {
                reason,
                sequence,
                usage: protocol::usage(
                    input_tokens,
                    output_tokens,
                    r.settings.simulated_cache,
                    read,
                ),
            },
        )
        .await
    }
}
