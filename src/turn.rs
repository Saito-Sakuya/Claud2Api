use crate::{
    engine::{Engine, Output, Reservation},
    error::{Fault, Result},
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
        r: &Reservation,
        plan: &mut AttemptPlan,
        tx: &mpsc::Sender<Output>,
    ) -> Result<(Input, usize)> {
        let settings = &r.settings;
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
        let prompt = projected.prompt(&protocol_nonce);
        let input_tokens = protocol::estimate(&r.input.prompt(&protocol_nonce));
        if let Some(trace) = &r.scope.trace {
            trace.begin_usage(input_tokens);
        }
        let target = if plan.fresh {
            let target = r.lease.slot.client.create(&r.lease, &r.scope).await?;
            self.own(&r.lease.account_id, &target).await?;
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
        let mut completion = r
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
                    tools: json!([]),
                    thinking: "off",
                },
            )
            .await?;
        let assistant = completion.assistant.clone();
        let mut decoder = ToolDecoder::new(&r.input, &protocol_nonce);
        let mut blocks = Vec::new();
        let mut started = false;
        let mut generated = false;
        let mut reason = "end_turn".to_owned();
        let mut sequence = None;
        let mut raw_size = 0usize;
        let mut thinking_size = 0usize;
        let read = if r.settings.simulated_cache && !plan.fresh && r.session.cache_until > now() {
            r.session.cache_tokens.min(input_tokens)
        } else {
            0
        };
        loop {
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
            if kind == "content_block_delta" {
                if !event["delta"].is_object() {
                    return Err(Fault::upstream());
                }
                let text = event
                    .pointer("/delta/text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                raw_size += text.len();
                thinking_size += event
                    .pointer("/delta/thinking")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .len();
                if raw_size > 8 << 20 || thinking_size > 8 << 20 {
                    return Err(Fault::new(
                        502,
                        "api_error",
                        "upstream output exceeds limit",
                    ));
                }
                for block in decoder.feed(text)? {
                    Self::send(&r.scope, tx, Output::Block(block.clone())).await?;
                    protocol::merge_text(&mut blocks, block);
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
        drop(completion);
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
            state.pending.retain(|j|!(j.account_id==r.lease.account_id&&j.conversation_id==target));
            let current=state.sessions.get_mut(&r.session.id).ok_or_else(Fault::internal)?;
            current.account_id=r.lease.account_id.clone();
            current.conversation_id=target;
            current.parent_uuid=assistant;
            current.nonce=protocol_nonce;
            current.context_hash=r.input.context(&self.config.gateway_key);
            current.history=hashes;
            current.expires_at=now()+r.settings.session_ttl_seconds as i64;
            current.uncertain=false;
            current.active=false;
            current.cache_tokens=cache_tokens;
            current.cache_until=if r.settings.simulated_cache{now()+3600}else{0};
            current.context_estimate=units.min(32<<20);
            current.checkpoint=plan.checkpoint.clone();
            if plan.fresh{current.compaction_retry_after=0;}
            current.model=r.input.model.clone();
            Ok(())
        }).await?;
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
