1	use super::system_prompt::{
2	    RequestedSystemPrompt, read_session_or_init_meta_str, requested_system_prompt,
3	};
4	use super::test_hooks::{AttachPause, with_pause_at};
5	use super::*;
6	use crate::agent::config::TraceUploadEndpoints;
7	use crate::agent::config::{EndpointsConfig, ModelEntry};
8	use crate::agent::handlers::model_switch::SwitchContextWindow;
9	use crate::extensions::code_nav::CodeNavEligibility;
10	use std::num::NonZeroU64;
11	/// Build an unsigned JWT with a `tier` claim (header.payload.sig base64url).
12	fn jwt_with_tier(tier: u64) -> String {
13	    use base64::Engine;
14	    let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
15	    let header = enc.encode(br#"{"alg":"none"}"#);
16	    let payload = enc.encode(format!(r#"{{"tier":{tier}}}"#).as_bytes());
17	    format!("{header}.{payload}.sig")
18	}
19	#[test]
20	fn jwt_tier_claim_maps_free_and_paid() {
21	    assert_eq!(jwt_tier_claim(&jwt_with_tier(0)).as_deref(), Some("free"));
22	    assert_eq!(
23	        jwt_tier_claim(&jwt_with_tier(1)).as_deref(),
24	        Some("supergrok")
25	    );
26	    assert_eq!(
27	        jwt_tier_claim(&jwt_with_tier(2)).as_deref(),
28	        Some("x_basic")
29	    );
30	    assert_eq!(
31	        jwt_tier_claim(&jwt_with_tier(3)).as_deref(),
32	        Some("x_premium")
33	    );
34	    assert_eq!(
35	        jwt_tier_claim(&jwt_with_tier(4)).as_deref(),
36	        Some("x_premium_plus")
37	    );
38	    assert_eq!(
39	        jwt_tier_claim(&jwt_with_tier(5)).as_deref(),
40	        Some("supergrok_heavy")
41	    );
42	    assert_eq!(
43	        jwt_tier_claim(&jwt_with_tier(6)).as_deref(),
44	        Some("supergrok_lite")
45	    );
46	    assert_eq!(
47	        jwt_tier_claim(&jwt_with_tier(7)).as_deref(),
48	        Some("supergrok_plus")
49	    );
50	    assert_eq!(jwt_tier_claim(&jwt_with_tier(9)).as_deref(), Some("9"));
51	    assert_eq!(jwt_tier_claim(&jwt_with_tier(99)).as_deref(), Some("99"));
52	}
53	fn auth_with_mode(mode: xai_grok_login::AuthMode, key: &str) -> xai_grok_login::GrokAuth {
54	    xai_grok_login::GrokAuth {
55	        key: key.into(),
56	        auth_mode: mode,
57	        user_id: "u".into(),
58	        coding_data_retention_opt_out: false,
59	        ..xai_grok_login::GrokAuth::default()
60	    }
61	}
62	#[test]
63	fn resolve_subscription_tier_prefers_display_then_api_key_then_jwt() {
64	    assert_eq!(
65	        resolve_subscription_tier_for_telemetry(Some("Free".into()), None).as_deref(),
66	        Some("Free")
67	    );
68	    let api = auth_with_mode(xai_grok_login::AuthMode::ApiKey, "xai-not-a-jwt");
69	    assert_eq!(
70	        resolve_subscription_tier_for_telemetry(Some("  ".into()), Some(&api)).as_deref(),
71	        Some("api_key")
72	    );
73	    assert_eq!(
74	        resolve_subscription_tier_for_telemetry(None, Some(&api)).as_deref(),
75	        Some("api_key")
76	    );
77	    let oauth = auth_with_mode(xai_grok_login::AuthMode::Oidc, &jwt_with_tier(0));
78	    assert_eq!(
79	        resolve_subscription_tier_for_telemetry(None, Some(&oauth)).as_deref(),
80	        Some("free")
81	    );
82	    assert_ne!(
83	        resolve_subscription_tier_for_telemetry(None, Some(&api)).as_deref(),
84	        Some("free")
85	    );
86	}
87	/// Maps the JWT tier claim to the `/user` tier name to gate the post-unblock catalog refresh (a stale older paid claim must not skip retry).
88	#[test]
89	fn jwt_claim_matches_user_subscription_tier_known_pairs() {
90	    let cases = [
91	        ("supergrok", "GrokPro"),
92	        ("x_basic", "XBasic"),
93	        ("x_premium", "XPremium"),
94	        ("x_premium_plus", "XPremiumPlus"),
95	        ("supergrok_heavy", "SuperGrokPro"),
96	        ("9", "EnterpriseMystery"),
97	        ("supergrok_lite", "SuperGrokLite"),
98	        ("supergrok_plus", "SuperGrokPlus"),
99	    ];
100	    for (claim, user_tier) in cases {
101	        assert!(
102	            jwt_claim_matches_user_subscription_tier(claim, user_tier),
103	            "{claim} should match {user_tier}"
104	        );
105	    }
106	}
107	#[test]
108	fn jwt_claim_matches_user_subscription_tier_rejects_stale_and_unknown() {
109	    assert!(!jwt_claim_matches_user_subscription_tier(
110	        "x_basic",
111	        "SuperGrokPro"
112	    ));
113	    assert!(!jwt_claim_matches_user_subscription_tier(
114	        "supergrok",
115	        "SuperGrokPro"
116	    ));
117	    assert!(!jwt_claim_matches_user_subscription_tier(
118	        "supergrok",
119	        "SuperGrokPlus"
120	    ));
121	    assert!(!jwt_claim_matches_user_subscription_tier(
122	        "supergrok_heavy",
123	        "SuperGrokPlus"
124	    ));
125	    assert!(!jwt_claim_matches_user_subscription_tier("free", "GrokPro"));
126	    assert!(!jwt_claim_matches_user_subscription_tier("", "XPremium"));
127	    assert!(!jwt_claim_matches_user_subscription_tier(
128	        "supergrok_heavy",
129	        "EnterpriseMystery"
130	    ));
131	    assert!(!jwt_claim_matches_user_subscription_tier(
132	        "0",
133	        "EnterpriseMystery"
134	    ));
135	}
136	/// Single-flight flag must clear on Drop even if the retry task panics / aborts mid-backoff (guards against the flag stuck true forever).
137	#[test]
138	fn post_unblock_jwt_retry_in_flight_guard_clears_on_drop() {
139	    use std::sync::Arc;
140	    use std::sync::atomic::{AtomicBool, Ordering};
141	    let flag = Arc::new(AtomicBool::new(true));
142	    {
143	        let _guard = PostUnblockJwtRetryInFlightGuard { flag: flag.clone() };
144	        assert!(flag.load(Ordering::Acquire));
145	    }
146	    assert!(
147	        !flag.load(Ordering::Acquire),
148	        "Drop must release post_unblock_jwt_retry_in_flight"
149	    );
150	    let flag = Arc::new(AtomicBool::new(true));
151	    let flag_for_catch = flag.clone();
152	    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
153	        let _guard = PostUnblockJwtRetryInFlightGuard {
154	            flag: flag_for_catch,
155	        };
156	        panic!("simulate retry task panic");
157	    }));
158	    assert!(result.is_err());
159	    assert!(
160	        !flag.load(Ordering::Acquire),
161	        "Drop must release flag on panic unwind"
162	    );
163	}
164	mod hunk_tracking_mode {
165	    use super::super::{plan_hunk_tracking, resolve_hunk_tracking_mode};
166	    use xai_hunk_tracker::TrackingMode;
167	    #[test]
168	    fn off_and_disabled_disable_tracking() {
169	        assert_eq!(resolve_hunk_tracking_mode(Some("off")), None);
170	        assert_eq!(resolve_hunk_tracking_mode(Some("disabled")), None);
171	    }
172	    #[test]
173	    fn matching_is_case_insensitive_and_trimmed() {
174	        assert_eq!(resolve_hunk_tracking_mode(Some("OFF")), None);
175	        assert_eq!(resolve_hunk_tracking_mode(Some("  Off ")), None);
176	        assert_eq!(resolve_hunk_tracking_mode(Some("DISABLED")), None);
177	        assert_eq!(
178	            resolve_hunk_tracking_mode(Some("Agent_Only")),
179	            Some(TrackingMode::AgentOnly)
180	        );
181	        assert_eq!(
182	            resolve_hunk_tracking_mode(Some(" ALL_DIRTY ")),
183	            Some(TrackingMode::AllDirty)
184	        );
185	    }
186	    #[test]
187	    fn recognized_modes_parse() {
188	        assert_eq!(
189	            resolve_hunk_tracking_mode(Some("agent_only")),
190	            Some(TrackingMode::AgentOnly)
191	        );
192	        assert_eq!(
193	            resolve_hunk_tracking_mode(Some("all_dirty")),
194	            Some(TrackingMode::AllDirty)
195	        );
196	    }
197	    #[test]
198	    fn parser_absent_returns_none_policy_defaults_in_plan() {
199	        assert_eq!(resolve_hunk_tracking_mode(None), None);
200	        assert_eq!(resolve_hunk_tracking_mode(Some("")), None);
201	        assert_eq!(
202	            resolve_hunk_tracking_mode(Some("bogus")),
203	            Some(TrackingMode::AllDirty)
204	        );
205	    }
206	    #[test]
207	    fn plan_disables_actor_forward_and_loc_together() {
208	        for off in ["off", "disabled", "OFF"] {
209	            let plan = plan_hunk_tracking(Some(off));
210	            assert_eq!(plan.actor_mode, None, "{off} must not spawn the actor");
211	            assert!(!plan.enabled(), "{off} must disable the forward + LOC sink");
212	        }
213	    }
214	    #[test]
215	    fn plan_enables_actor_and_forward_for_active_modes() {
216	        for (mode, expected) in [
217	            ("agent_only", TrackingMode::AgentOnly),
218	            ("all_dirty", TrackingMode::AllDirty),
219	            ("bogus", TrackingMode::AllDirty),
220	        ] {
221	            let plan = plan_hunk_tracking(Some(mode));
222	            assert_eq!(plan.actor_mode, Some(expected));
223	            assert!(plan.enabled());
224	        }
225	        let plan = plan_hunk_tracking(None);
226	        assert_eq!(plan.actor_mode, None);
227	        assert!(!plan.enabled());
228	    }
229	}
230	mod capture {
231	    use tokio::sync::mpsc;
232	    use tracing::Subscriber;
233	    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
234	    use tracing_subscriber::registry::LookupSpan;
235	    pub(crate) struct CapturedEvent {
236	        pub level: tracing::Level,
237	        pub fields: String,
238	    }
239	    pub(crate) struct Captured {
240	        pub events_rx: mpsc::UnboundedReceiver<CapturedEvent>,
241	        _guard: tracing::subscriber::DefaultGuard,
242	    }
243	    pub(crate) fn capture() -> Captured {
244	        let (tx, rx) = mpsc::unbounded_channel();
245	        let subscriber = tracing_subscriber::registry().with(CaptureLayer { tx });
246	        let guard = tracing::subscriber::set_default(subscriber);
247	        Captured {
248	            events_rx: rx,
249	            _guard: guard,
250	        }
251	    }
252	    struct CaptureLayer {
253	        tx: mpsc::UnboundedSender<CapturedEvent>,
254	    }
255	    impl<S> Layer<S> for CaptureLayer
256	    where
257	        S: Subscriber + for<'a> LookupSpan<'a>,
258	    {
259	        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
260	            let mut v = Visitor::default();
261	            event.record(&mut v);
262	            let _ = self.tx.send(CapturedEvent {
263	                level: *event.metadata().level(),
264	                fields: v.out,
265	            });
266	        }
267	    }
268	    #[derive(Default)]
269	    struct Visitor {
270	        out: String,
271	    }
272	    impl tracing::field::Visit for Visitor {
273	        fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
274	            if !self.out.is_empty() {
275	                self.out.push(' ');
276	            }
277	            self.out.push_str(f.name());
278	            self.out.push('=');
279	            self.out.push_str(&format!("{v:?}"));
280	        }
281	        fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
282	            if !self.out.is_empty() {
283	                self.out.push(' ');
284	            }
285	            self.out.push_str(f.name());
286	            self.out.push('=');
287	            self.out.push_str(v);
288	        }
289	    }
290	}
291	#[test]
292	fn warn_on_missing_parent_session_emits_when_session_absent() {
293	    let captured = capture::capture();
294	    warn_on_missing_parent_session_for_validate_type("ghost-session", false);
295	    let mut rx = captured.events_rx;
296	    let mut saw = false;
297	    while let Ok(event) = rx.try_recv() {
298	        if event.level == tracing::Level::WARN
299	            && event
300	                .fields
301	                .contains("ValidateType received for unknown parent session")
302	            && event.fields.contains("parent_session_id=ghost-session")
303	        {
304	            saw = true;
305	            break;
306	        }
307	    }
308	    assert!(saw, "warn must fire");
309	}
310	#[test]
311	fn warn_on_missing_parent_session_silent_when_session_present() {
312	    let captured = capture::capture();
313	    warn_on_missing_parent_session_for_validate_type("real-session", true);
314	    let mut rx = captured.events_rx;
315	    assert!(rx.try_recv().is_err());
316	}
317	#[tokio::test(flavor = "current_thread")]
318	async fn broadcast_refresh_skill_baseline_sends_one_message_per_sender() {
319	    let mut receivers = Vec::new();
320	    let mut senders = Vec::new();
321	    for _ in 0..3 {
322	        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
323	        senders.push(tx);
324	        receivers.push(rx);
325	    }
326	    MvpAgent::broadcast_refresh_skill_baseline(senders);
327	    for mut rx in receivers {
328	        assert!(matches!(
329	            rx.try_recv(),
330	            Ok(crate::session::SessionCommand::RefreshSkillBaseline)
331	        ));
332	        assert!(
333	            rx.try_recv().is_err(),
334	            "broadcast must send exactly one message per sender",
335	        );
336	    }
337	}
338	#[tokio::test(flavor = "current_thread")]
339	async fn broadcast_refresh_skill_baseline_tolerates_dropped_receiver() {
340	    let (tx_alive, mut rx_alive) = tokio::sync::mpsc::unbounded_channel();
341	    let (tx_dead, rx_dead) = tokio::sync::mpsc::unbounded_channel();
342	    drop(rx_dead);
343	    MvpAgent::broadcast_refresh_skill_baseline(vec![tx_alive, tx_dead]);
344	    assert!(matches!(
345	        rx_alive.try_recv(),
346	        Ok(crate::session::SessionCommand::RefreshSkillBaseline)
347	    ));
348	}
349	#[test]
350	fn settings_allow_access_none_settings_is_allowed() {
351	    assert!(settings_allow_access(None));
352	}
353	#[test]
354	fn settings_allow_access_true_is_allowed() {
355	    let rs = crate::util::config::RemoteSettings {
356	        allow_access: Some(true),
357	        ..Default::default()
358	    };
359	    assert!(settings_allow_access(Some(&rs)));
360	}
361	#[test]
362	fn settings_allow_access_false_is_blocked() {
363	    let rs = crate::util::config::RemoteSettings {
364	        allow_access: Some(false),
365	        ..Default::default()
366	    };
367	    assert!(!settings_allow_access(Some(&rs)));
368	}
369	#[test]
370	fn settings_allow_access_field_absent_is_allowed() {
371	    let rs = crate::util::config::RemoteSettings {
372	        allow_access: None,
373	        ..Default::default()
374	    };
375	    assert!(settings_allow_access(Some(&rs)));
376	}
377	/// Build a synthetic harness `task` call/result pair carrying the `<subagent_result>` footer, mirroring what the verifier/planner record.
378	fn harness_pair(id: &str) -> Vec<xai_grok_sampling_types::conversation::ConversationItem> {
379	    use xai_grok_sampling_types::ToolCall;
380	    use xai_grok_sampling_types::conversation::ConversationItem;
381	    vec![
382	        ConversationItem::assistant_tool_calls(vec![ToolCall {
383	            id: id.into(),
384	            name: "task".into(),
385	            arguments: "{}".into(),
386	        }]),
387	        ConversationItem::tool_result(id, "<subagent_result>\nsubagent_id: skeptic-1"),
388	    ]
389	}
390	#[test]
391	fn retained_resources_empty_without_optional_cache() {
392	    let resources = RetainedResources::default();
393	    assert!(resources.is_empty());
394	}
395	#[test]
396	fn retained_resources_with_turn_number_is_not_empty() {
397	    let resources = RetainedResources {
398	        turn_number: Some(1),
399	        ..Default::default()
400	    };
401	    assert!(!resources.is_empty());
402	}
403	#[tokio::test(flavor = "current_thread")]
404	async fn child_attempt_turn_numbers_start_at_zero_and_advance_monotonically() {
405	    let agent = build_minimal_agent_for_tests();
406	    let sid = acp::SessionId::new("child-attempt-turns");
407	    assert_eq!(agent.allocate_turn_number(&sid), 0);
408	    assert_eq!(agent.allocate_turn_number(&sid), 1);
409	    assert_eq!(agent.allocate_turn_number(&sid), 2);
410	    assert_eq!(agent.session_turn_number(&sid), Some(3));
411	    agent.release_subagent_turn_number(&sid);
412	    assert_eq!(agent.session_turn_number(&sid), None);
413	}
414	#[tokio::test(flavor = "current_thread")]
415	async fn releasing_subagent_turn_number_keeps_resident_session() {
416	    let agent = build_minimal_agent_for_tests();
417	    let sid = acp::SessionId::new("child-turn-resident");
418	    agent.insert_resident(&sid, make_test_handle("test-model", false, None));
419	    assert_eq!(agent.allocate_turn_number(&sid), 0);
420	    agent.release_subagent_turn_number(&sid);
421	    assert_eq!(agent.session_turn_number(&sid), None);
422	    assert!(agent.is_resident(&sid));
423	}
424	#[tokio::test(flavor = "current_thread")]
425	async fn first_subagent_turn_allocation_does_not_walk_the_sessions_index() {
426	    crate::session::persistence::set_find_summary_by_session_id_forbidden(true);
427	    struct Reset;
428	    impl Drop for Reset {
429	        fn drop(&mut self) {
430	            crate::session::persistence::set_find_summary_by_session_id_forbidden(false);
431	        }
432	    }
433	    let _reset = Reset;
434	    let agent = build_minimal_agent_for_tests();
435	    let sid = acp::SessionId::new("child-with-no-live-counter");
436	    assert_eq!(agent.allocate_turn_number(&sid), 0);
437	    assert_eq!(agent.session_turn_number(&sid), Some(1));
438	}
439	/// Agent-side upload path: each drained harness turn takes a distinct, monotonic turn number that CONTINUES past the user turn.
440	/// It advances the per-session counter, persisted via exactly one `SetNextTraceTurn`.
441	/// This is what makes each sibling `turn_{N}` reachable: without the advance every harness turn would clobber the same GCS path.
442	#[tokio::test(flavor = "current_thread")]
443	async fn upload_harness_trace_turns_numbers_siblings_and_persists_counter() {
444	    let agent = build_minimal_agent_for_tests();
445	    {
446	        let mut cfg = agent.cfg.borrow_mut();
447	        cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Enabled);
448	        cfg.telemetry.trace_upload = Some(true);
449	        cfg.endpoints.trace_upload_bucket = Some("gs://harness-trace-test".to_string());
450	    }
451	    let sid = acp::SessionId::new("harness-upload-sess");
452	    let info = crate::session::info::Info {
453	        id: sid.clone(),
454	        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
455	    };
456	    let mut handle = make_test_handle("test-model", false, None);
457	    handle.info = info.clone();
458	    let queue_home = tempfile::tempdir().unwrap();
459	    let queue_cfg = crate::session::repo_changes::TraceExportConfig {
460	        bucket_url: Some("gs://harness-trace-test".to_string()),
461	        service_account_key: None,
462	        prefix_dir: None,
463	        gcs_prefix: None,
464	        absolute_paths: false,
465	        archive_name_override: None,
466	        upload_method: crate::session::repo_changes::UploadMethod::Direct {
467	            service_account_key: None,
468	        },
469	    };
470	    let queue = crate::upload::trace::spawn_upload_queue(
471	        queue_home.path(),
472	        &queue_cfg,
473	        Some(xai_grok_version::VERSION),
474	        agent.auth_manager.clone(),
475	    );
476	    let _ = handle.upload_queue.set(queue);
477	    agent.insert_resident(&sid, handle);
478	    for _ in 0..3 {
479	        agent.allocate_turn_number(&sid);
480	    }
481	    assert_eq!(agent.session_turn_number(&sid), Some(3));
482	    let built = agent
483	        .build_harness_trace_uploads(
484	            &sid,
485	            &info,
486	            "test-model",
487	            3,
488	            vec![harness_pair("a"), harness_pair("b")],
489	        )
490	        .await;
491	    if xai_grok_version::research_data_collection_forbidden() {
492	        // Privacy build: trace upload is hard-off, so no harness trace
493	        // uploads may be built; the counter-advance half is asserted below.
494	        assert!(built.is_empty(), "privacy build: no harness trace uploads");
495	    } else {
496	        let numbers: Vec<u64> = built.iter().map(|(_, m, _)| m.turn_number).collect();
497	        assert_eq!(numbers, vec![3, 4], "siblings take base, base+1");
498	        assert!(
499	            built.iter().all(|(_, m, _)| m.model == "test-model"),
500	            "harness metadata carries the requested model alias",
501	        );
502	    }
503	    if xai_grok_version::research_data_collection_forbidden() {
504	        // Privacy build: uploads are hard-off, so the drain is a no-op that
505	        // neither advances the counter nor persists a SetNextTraceTurn.
506	        let (cmd_tx, mut cmd_rx) =
507	            tokio::sync::mpsc::unbounded_channel::<crate::session::SessionCommand>();
508	        agent
509	            .upload_harness_trace_turns(
510	                &sid,
511	                &info,
512	                &cmd_tx,
513	                "test-model",
514	                vec![harness_pair("a"), harness_pair("b")],
515	            )
516	            .await;
517	        assert_eq!(
518	            agent.session_turn_number(&sid),
519	            Some(3),
520	            "privacy build: no uploads means no counter advance",
521	        );
522	        assert!(
523	            cmd_rx.try_recv().is_err(),
524	            "privacy build: no SetNextTraceTurn may be persisted",
525	        );
526	        return;
527	    }
528	    let (cmd_tx, mut cmd_rx) =
529	        tokio::sync::mpsc::unbounded_channel::<crate::session::SessionCommand>();
530	    agent
531	        .upload_harness_trace_turns(
532	            &sid,
533	            &info,
534	            &cmd_tx,
535	            "test-model",
536	            vec![harness_pair("a"), harness_pair("b")],
537	        )
538	        .await;
539	    assert_eq!(
540	        agent.session_turn_number(&sid),
541	        Some(5),
542	        "two siblings advance the counter by two from the user turn",
543	    );
544	    let mut persisted = Vec::new();
545	    while let Ok(cmd) = cmd_rx.try_recv() {
546	        if let crate::session::SessionCommand::SetNextTraceTurn {
547	            next_trace_turn, ..
548	        } = cmd
549	        {
550	            persisted.push(next_trace_turn);
551	        }
552	    }
553	    assert_eq!(
554	        persisted,
555	        vec![5],
556	        "persist the advanced counter once, ahead of the spawned uploads",
557	    );
558	}
559	/// With trace upload disabled the agent-side path must NOT burn a turn number or persist a counter (and spawns no upload).
560	/// The buffer-clearing half of the drain is the caller's `TakeHarnessTraceTurns`; this guards the upload function's uploads-disabled branch.
561	#[tokio::test(flavor = "current_thread")]
562	async fn upload_harness_trace_turns_uploads_disabled_does_not_burn_counter() {
563	    let agent = build_minimal_agent_for_tests();
564	    let sid = acp::SessionId::new("harness-disabled-sess");
565	    let info = crate::session::info::Info {
566	        id: sid.clone(),
567	        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
568	    };
569	    let (cmd_tx, mut cmd_rx) =
570	        tokio::sync::mpsc::unbounded_channel::<crate::session::SessionCommand>();
571	    agent
572	        .upload_harness_trace_turns(&sid, &info, &cmd_tx, "test-model", vec![harness_pair("a")])
573	        .await;
574	    assert_eq!(
575	        agent.session_turn_number(&sid),
576	        None,
577	        "uploads-disabled skip must not consume a turn number",
578	    );
579	    assert!(
580	        cmd_rx.try_recv().is_err(),
581	        "uploads-disabled path must not persist a counter",
582	    );
583	}
584	/// Guards three facts of the per-harness-turn manifest. (1) Every turn's ctx carries a FRESH `artifact_tracker`, so turn 1 never inherits turn 0's recorded artifacts.
585	/// (2) Recording the turn's metadata and turn_messages yields a manifest listing exactly those two. (3) `fully_uploaded` is true iff neither failed.
586	#[tokio::test(flavor = "current_thread")]
587	async fn upload_harness_trace_turns_build_per_turn_manifest() {
588	    use crate::upload::manifest::{
589	        ArtifactResult, ArtifactStatus, build_manifest, record_artifact, resolve_upload_method,
590	    };
591	    let agent = build_minimal_agent_for_tests();
592	    {
593	        let mut cfg = agent.cfg.borrow_mut();
594	        cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Enabled);
595	        cfg.telemetry.trace_upload = Some(true);
596	        cfg.endpoints.trace_upload_bucket = Some("gs://harness-trace-test".to_string());
597	    }
598	    let sid = acp::SessionId::new("harness-manifest-sess");
599	    let info = crate::session::info::Info {
600	        id: sid.clone(),
601	        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
602	    };
603	    let mut handle = make_test_handle("test-model", false, None);
604	    handle.info = info.clone();
605	    let queue_home = tempfile::tempdir().unwrap();
606	    let queue_cfg = crate::session::repo_changes::TraceExportConfig {
607	        bucket_url: Some("gs://harness-trace-test".to_string()),
608	        service_account_key: None,
609	        prefix_dir: None,
610	        gcs_prefix: None,
611	        absolute_paths: false,
612	        archive_name_override: None,
613	        upload_method: crate::session::repo_changes::UploadMethod::Direct {
614	            service_account_key: None,
615	        },
616	    };
617	    let queue = crate::upload::trace::spawn_upload_queue(
618	        queue_home.path(),
619	        &queue_cfg,
620	        Some(xai_grok_version::VERSION),
621	        agent.auth_manager.clone(),
622	    );
623	    let _ = handle.upload_queue.set(queue);
624	    agent.insert_resident(&sid, handle);
625	    let built = agent
626	        .build_harness_trace_uploads(
627	            &sid,
628	            &info,
629	            "test-model",
630	            0,
631	            vec![harness_pair("a"), harness_pair("b")],
632	        )
633	        .await;
634	    if xai_grok_version::research_data_collection_forbidden() {
635	        assert!(
636	            built.is_empty(),
637	            "privacy build: trace context is never obtained (hard-off)"
638	        );
639	        return;
640	    }
641	    assert_eq!(
642	        built.len(),
643	        2,
644	        "both harness turns obtained a trace context"
645	    );
646	    let Some((ctx0, _, _)) = built.first() else {
647	        panic!("expected two harness turns");
648	    };
649	    record_artifact(
650	        &ctx0.artifact_tracker,
651	        "metadata.json",
652	        ArtifactResult::Succeeded,
653	    );
654	    record_artifact(
655	        &ctx0.artifact_tracker,
656	        "turn_messages.json",
657	        ArtifactResult::Succeeded,
658	    );
659	    let m0 = build_manifest(
660	        &ctx0.artifact_tracker,
661	        resolve_upload_method(&ctx0.gcs_config),
662	        None,
663	    );
664	    assert!(matches!(
665	        m0.artifacts.get("metadata.json"),
666	        Some(ArtifactStatus::Succeeded)
667	    ));
668	    assert!(matches!(
669	        m0.artifacts.get("turn_messages.json"),
670	        Some(ArtifactStatus::Succeeded)
671	    ));
672	    assert!(m0.fully_uploaded, "both succeeded → fully_uploaded");
673	    let Some((ctx1, _, _)) = built.get(1) else {
674	        panic!("expected two harness turns");
675	    };
676	    let before = build_manifest(
677	        &ctx1.artifact_tracker,
678	        resolve_upload_method(&ctx1.gcs_config),
679	        None,
680	    );
681	    assert!(
682	        before.artifacts.is_empty(),
683	        "per-turn tracker: turn 1 must not inherit turn 0's artifacts",
684	    );
685	    record_artifact(
686	        &ctx1.artifact_tracker,
687	        "metadata.json",
688	        ArtifactResult::Succeeded,
689	    );
690	    record_artifact(
691	        &ctx1.artifact_tracker,
692	        "turn_messages.json",
693	        ArtifactResult::Failed {
694	            reason: "upload_failed",
695	            error: None,
696	        },
697	    );
698	    let m1 = build_manifest(
699	        &ctx1.artifact_tracker,
700	        resolve_upload_method(&ctx1.gcs_config),
701	        None,
702	    );
703	    assert!(
704	        !m1.fully_uploaded,
705	        "a failed turn_messages flips fully_uploaded",
706	    );
707	    assert_eq!(m1.artifacts.len(), 2, "no cross-turn contamination");
708	}
709	/// With no overrides and model_agent_type = None, the default agent is used.
710	#[test]
711	#[serial_test::serial]
712	fn resolve_agent_definition_defaults_to_grok_build() {
713	    let prev = std::env::var("GROK_AGENT").ok();
714	    unsafe {
715	        std::env::remove_var("GROK_AGENT");
716	    }
717	    let tmp = tempfile::tempdir().unwrap();
718	    let def = MvpAgent::resolve_agent_definition(
719	        tmp.path(),
720	        None,
721	        &config::AgentSelectionConfig::default(),
722	        None,
723	        None,
724	    );
725	    assert_eq!(def.name, config::DEFAULT_AGENT_TYPE);
726	    if let Some(v) = prev {
727	        unsafe { std::env::set_var("GROK_AGENT", v) }
728	    }
729	}
730	/// When model_agent_type = Some("codex"), the codex agent is selected even though the default chain would return grok-build.
731	#[test]
732	#[serial_test::serial]
733	fn resolve_agent_definition_model_agent_type_overrides_default() {
734	    let prev = std::env::var("GROK_AGENT").ok();
735	    unsafe {
736	        std::env::remove_var("GROK_AGENT");
737	    }
738	    let tmp = tempfile::tempdir().unwrap();
739	    let def = MvpAgent::resolve_agent_definition(
740	        tmp.path(),
741	        None,
742	        &config::AgentSelectionConfig::default(),
743	        None,
744	        Some("codex"),
745	    );
746	    assert_eq!(def.name, "codex");
747	    if let Some(v) = prev {
748	        unsafe { std::env::set_var("GROK_AGENT", v) }
749	    }
750	}
751	/// When model_agent_type is None, the chain-resolved default agent is NOT overridden.
752	/// In leader mode a session whose model has no agent_type must get the default agent, not a stale value from a different client's model.
753	#[test]
754	#[serial_test::serial]
755	fn resolve_agent_definition_none_agent_type_does_not_override() {
756	    let prev = std::env::var("GROK_AGENT").ok();
757	    unsafe {
758	        std::env::remove_var("GROK_AGENT");
759	    }
760	    let tmp = tempfile::tempdir().unwrap();
761	    let def = MvpAgent::resolve_agent_definition(
762	        tmp.path(),
763	        None,
764	        &config::AgentSelectionConfig::default(),
765	        None,
766	        None,
767	    );
768	    assert_eq!(def.name, config::DEFAULT_AGENT_TYPE);
769	    if let Some(v) = prev {
770	        unsafe { std::env::set_var("GROK_AGENT", v) }
771	    }
772	}
773	/// Regression for the web-client devbox bug: an ACP profile must win when the model's `agent_type` is the default value.
774	#[test]
775	#[serial_test::serial]
776	fn resolve_agent_definition_acp_profile_wins_when_model_agent_type_is_default() {
777	    let prev = std::env::var("GROK_AGENT").ok();
778	    unsafe {
779	        std::env::remove_var("GROK_AGENT");
780	    }
781	    let tmp = tempfile::tempdir().unwrap();
782	    let acp_profile = xai_grok_agent::AgentDefinition::from_json(&serde_json::json!(
783	        { "name" : "custom-devbox-profile", "description" :
784	        "Custom devbox profile", "systemPrompt" :
785	        "You are a custom-configured devbox agent.", }
786	    ))
787	    .expect("agent definition must parse");
788	    let def = MvpAgent::resolve_agent_definition(
789	        tmp.path(),
790	        None,
791	        &config::AgentSelectionConfig::default(),
792	        Some(acp_profile),
793	        Some(config::DEFAULT_AGENT_TYPE),
794	    );
795	    assert_eq!(
796	        def.name, "custom-devbox-profile",
797	        "ACP _meta.agentProfile must win when model_agent_type is the default value"
798	    );
799	    if let Some(v) = prev {
800	        unsafe { std::env::set_var("GROK_AGENT", v) }
801	    }
802	}
803	/// Regression: `DEFAULT_AGENT_TYPE` flipped to `grok-build-plan`.
804	/// Models in the catalog that still declare `agent_type = "grok-build"` explicitly must NOT preempt an ACP profile.
805	/// Any value in the `grok-build*` family is the stock harness with no strict requirement.
806	#[test]
807	#[serial_test::serial]
808	fn resolve_agent_definition_acp_profile_wins_for_explicit_grok_build_family() {
809	    let prev = std::env::var("GROK_AGENT").ok();
810	    unsafe {
811	        std::env::remove_var("GROK_AGENT");
812	    }
813	    let tmp = tempfile::tempdir().unwrap();
814	    let acp_profile = xai_grok_agent::AgentDefinition::from_json(&serde_json::json!({
815	        "name": "custom-devbox-profile",
816	        "description": "Custom devbox profile",
817	    }))
818	    .expect("agent definition must parse");
819	    for family_variant in ["grok-build", "grok-build-plan", "grok-build-concise"] {
820	        let def = MvpAgent::resolve_agent_definition(
821	            tmp.path(),
822	            None,
823	            &config::AgentSelectionConfig::default(),
824	            Some(acp_profile.clone()),
825	            Some(family_variant),
826	        );
827	        assert_eq!(
828	            def.name, "custom-devbox-profile",
829	            "ACP profile must win for grok-build family variant `{family_variant}`"
830	        );
831	    }
832	    if let Some(v) = prev {
833	        unsafe { std::env::set_var("GROK_AGENT", v) }
834	    }
835	}
836	/// A non-strict (stock / vision-capable) model leaves the template alone, so such models keep native image input.
837	#[test]
838	fn inherited_harness_template_skips_nonstrict_model() {
839	    use xai_grok_agent::prompt::user_message::UserMessageTemplate;
840	    let tmp = tempfile::tempdir().unwrap();
841	    assert!(
842	        inherited_harness_template(
843	            &UserMessageTemplate::Default,
844	            Some(config::DEFAULT_AGENT_TYPE),
845	            tmp.path(),
846	        )
847	        .is_none()
848	    );
849	}
850	/// An explicit (non-default) template is never overridden; inheritance only fills in the default.
851	#[test]
852	fn inherited_harness_template_respects_explicit_template() {
853	    use xai_grok_agent::prompt::user_message::UserMessageTemplate;
854	    let tmp = tempfile::tempdir().unwrap();
855	    let explicit = UserMessageTemplate::Custom("MY CUSTOM TEMPLATE".to_owned());
856	    assert!(inherited_harness_template(&explicit, Some("cursor"), tmp.path()).is_none());
857	}
858	/// CLI `--agent-profile` wins when model_agent_type is the default (also shadowed by the same regression).
859	#[test]
860	#[serial_test::serial]
861	fn resolve_agent_definition_cli_agent_profile_wins_when_model_agent_type_is_default() {
862	    let prev = std::env::var("GROK_AGENT").ok();
863	    unsafe {
864	        std::env::remove_var("GROK_AGENT");
865	    }
866	    let tmp = tempfile::tempdir().unwrap();
867	    let profile_path = tmp.path().join("cli-profile.md");
868	    std::fs::write(
869	        &profile_path,
870	        "---\nname: cli-profile\ndescription: cli test\n---\nYou are a CLI profile.\n",
871	    )
872	    .unwrap();
873	    let def = MvpAgent::resolve_agent_definition(
874	        tmp.path(),
875	        Some(&profile_path),
876	        &config::AgentSelectionConfig::default(),
877	        None,
878	        Some(config::DEFAULT_AGENT_TYPE),
879	    );
880	    assert_eq!(def.name, "cli-profile");
881	    if let Some(v) = prev {
882	        unsafe { std::env::set_var("GROK_AGENT", v) }
883	    }
884	}
885	/// Agent profile with `model: Override(id)` preserves the field through resolution.
886	#[test]
887	#[serial_test::serial]
888	fn resolve_agent_definition_agent_profile_with_model_override() {
889	    let prev = std::env::var("GROK_AGENT").ok();
890	    unsafe {
891	        std::env::remove_var("GROK_AGENT");
892	    }
893	    let tmp = tempfile::tempdir().unwrap();
894	    let agents_dir = tmp.path().join(".grok").join("agents");
895	    std::fs::create_dir_all(&agents_dir).unwrap();
896	    std::fs::write(
897	            agents_dir.join("test-architect.md"),
898	            "---\nname: test-architect\ndescription: test\nmodel: test-model-123\n---\nYou are a test.\n",
899	        )
900	        .unwrap();
901	    let agent_config = config::AgentSelectionConfig {
902	        name: Some("test-architect".to_string()),
903	        definition: None,
904	        system_prompt_label: None,
905	    };
906	    let def = MvpAgent::resolve_agent_definition(tmp.path(), None, &agent_config, None, None);
907	    assert_eq!(def.name, "test-architect");
908	    assert_eq!(
909	        def.model,
910	        xai_grok_agent::config::ModelOverride::Override("test-model-123".to_string()),
911	        "agent profile model override must be preserved through resolution"
912	    );
913	    match prev {
914	        Some(v) => unsafe { std::env::set_var("GROK_AGENT", v) },
915	        None => unsafe { std::env::remove_var("GROK_AGENT") },
916	    }
917	}
918	#[test]
919	fn read_session_or_init_meta_str_prefers_session_meta() {
920	    let session = serde_json::json!({ "rules": "from-session" });
921	    let init = serde_json::json!({ "rules": "from-init" });
922	    assert_eq!(
923	        read_session_or_init_meta_str(session.as_object(), init.as_object(), "rules"),
924	        Some("from-session"),
925	    );
926	}
927	#[test]
928	fn read_session_or_init_meta_str_falls_back_to_init_meta() {
929	    let session = serde_json::json!({ "other": "x" });
930	    let init = serde_json::json!({ "rules": "from-init" });
931	    assert_eq!(
932	        read_session_or_init_meta_str(session.as_object(), init.as_object(), "rules"),
933	        Some("from-init"),
934	    );
935	    assert_eq!(
936	        read_session_or_init_meta_str(None, init.as_object(), "rules"),
937	        Some("from-init"),
938	    );
939	}
940	#[test]
941	fn parse_session_plugin_dirs_filters_and_dedupes() {
942	    use crate::session::session_create_prefetch::parse_session_plugin_dirs;
943	    let tmp = tempfile::tempdir().unwrap();
944	    let dir = dunce::canonicalize(tmp.path()).unwrap().join("plugin");
945	    std::fs::create_dir(&dir).unwrap();
946	    let file = tmp.path().join("file.txt");
947	    std::fs::write(&file, "x").unwrap();
948	    let meta = serde_json::json!({
949	        "pluginDirs": [
950	            dir.to_string_lossy(),          // kept
951	            dir.to_string_lossy(),          // duplicate, deduped
952	            file.to_string_lossy(),         // not a directory, skipped
953	            "relative/path",                // not absolute, skipped
954	            42,                             // not a string, skipped
955	        ]
956	    });
957	    assert_eq!(
958	        &[dir],
959	        parse_session_plugin_dirs(meta.as_object()).as_paths()
960	    );
961	    assert!(parse_session_plugin_dirs(None).as_paths().is_empty());
962	    assert!(
963	        parse_session_plugin_dirs(serde_json::json!({}).as_object())
964	            .as_paths()
965	            .is_empty()
966	    );
967	}
968	#[test]
969	fn read_session_or_init_meta_str_returns_none_when_absent() {
970	    assert_eq!(read_session_or_init_meta_str(None, None, "rules"), None,);
971	    let session = serde_json::json!({ "other": "x" });
972	    assert_eq!(
973	        read_session_or_init_meta_str(session.as_object(), None, "rules"),
974	        None,
975	    );
976	}
977	#[test]
978	fn read_session_or_init_meta_str_ignores_non_string_values() {
979	    let session = serde_json::json!({ "rules": 42 });
980	    let init = serde_json::json!({ "rules": "from-init" });
981	    assert_eq!(
982	        read_session_or_init_meta_str(session.as_object(), init.as_object(), "rules"),
983	        Some("from-init"),
984	    );
985	}
986	#[test]
987	fn startup_hints_from_meta_prefers_session_request_over_init() {
988	    let session = serde_json::json!({
989	        "startupHints": { "nonInteractive": true, "deliveryTools": ["srv__post"] }
990	    });
991	    let init = serde_json::json!({ "startupHints": { "nonInteractive": false } });
992	    let hints = startup_hints_from_meta(session.as_object(), init.as_object());
993	    assert!(hints.non_interactive);
994	    assert_eq!(hints.delivery_tools, vec!["srv__post"]);
995	    assert!(startup_hints_from_meta(None, session.as_object()).non_interactive);
996	}
997	#[test]
998	fn startup_hints_from_meta_session_object_wins_whole_not_merged() {
999	    let session = serde_json::json!({ "startupHints": { "isSubagent": true } });
1000	    let init = serde_json::json!({ "startupHints": { "nonInteractive": true } });
1001	    let hints = startup_hints_from_meta(session.as_object(), init.as_object());
1002	    assert!(hints.is_subagent);
1003	    assert!(!hints.non_interactive);
1004	}
1005	#[test]
1006	fn startup_hints_from_meta_adopts_session_traceparent_only() {
1007	    let tp = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
1008	    let meta = serde_json::json!({ "traceparent": tp });
1009	    assert_eq!(
1010	        startup_hints_from_meta(meta.as_object(), None)
1011	            .startup_traceparent
1012	            .borrow()
1013	            .as_deref(),
1014	        Some(tp)
1015	    );
1016	    assert!(
1017	        startup_hints_from_meta(None, meta.as_object())
1018	            .startup_traceparent
1019	            .borrow()
1020	            .is_none()
1021	    );
1022	}
1023	#[test]
1024	fn startup_hints_from_meta_unparseable_falls_through_then_defaults() {
1025	    let bad = serde_json::json!({ "startupHints": "yes" });
1026	    let init = serde_json::json!({ "startupHints": { "nonInteractive": true } });
1027	    assert!(startup_hints_from_meta(bad.as_object(), init.as_object()).non_interactive);
1028	    assert!(!startup_hints_from_meta(None, None).non_interactive);
1029	}
1030	#[test]
1031	fn system_prompt_override_from_meta_prefers_session_and_rejects_empty() {
1032	    let session = serde_json::json!({ "systemPromptOverride": "from session" });
1033	    let init = serde_json::json!({ "systemPromptOverride": "from init" });
1034	    assert_eq!(
1035	        system_prompt_override_from_meta(session.as_object(), init.as_object()),
1036	        Some("from session")
1037	    );
1038	    assert_eq!(
1039	        system_prompt_override_from_meta(None, init.as_object()),
1040	        Some("from init")
1041	    );
1042	    let empty = serde_json::json!({ "systemPromptOverride": "" });
1043	    assert_eq!(
1044	        system_prompt_override_from_meta(empty.as_object(), None),
1045	        None
1046	    );
1047	    assert_eq!(system_prompt_override_from_meta(None, None), None);
1048	}
1049	#[test]
1050	fn override_wins_over_rules() {
1051	    let cases = [
1052	        (
1053	            serde_json::json!({ "systemPromptOverride": "you are terse", "rules": "keep the marker" }),
1054	            Some(RequestedSystemPrompt::Override("you are terse")),
1055	        ),
1056	        (
1057	            serde_json::json!({ "systemPromptOverride": "you are terse" }),
1058	            Some(RequestedSystemPrompt::Override("you are terse")),
1059	        ),
1060	        (
1061	            serde_json::json!({ "rules": "keep the marker" }),
1062	            Some(RequestedSystemPrompt::Rules("keep the marker")),
1063	        ),
1064	        (serde_json::json!({}), None),
1065	    ];
1066	    for (init, expected) in cases {
1067	        assert_eq!(expected, requested_system_prompt(None, init.as_object()));
1068	    }
1069	}
1070	#[test]
1071	fn blank_override_is_no_override() {
1072	    let cases = [
1073	        (
1074	            serde_json::json!({ "systemPromptOverride": "  ", "rules": "keep the marker" }),
1075	            Some(RequestedSystemPrompt::Rules("keep the marker")),
1076	        ),
1077	        (serde_json::json!({ "systemPromptOverride": "" }), None),
1078	    ];
1079	    for (init, expected) in cases {
1080	        assert_eq!(expected, requested_system_prompt(None, init.as_object()));
1081	    }
1082	}
1083	#[test]
1084	fn enqueue_replace_system_prompt_override_sends_when_present() {
1085	    use crate::session::SessionCommand;
1086	    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
1087	    let session = serde_json::json!({ "systemPromptOverride": "from session" });
1088	    let init = serde_json::json!({ "systemPromptOverride": "from init" });
1089	    enqueue_replace_system_prompt_override(&tx, session.as_object(), init.as_object());
1090	    match rx.try_recv() {
1091	        Ok(SessionCommand::ReplaceSystemPrompt { system_prompt }) => {
1092	            assert_eq!(system_prompt, "from session", "session meta wins over init");
1093	        }
1094	        _ => panic!("expected a ReplaceSystemPrompt command"),
1095	    }
1096	}
1097	#[test]
1098	fn enqueue_replace_system_prompt_override_noop_when_absent_or_empty() {
1099	    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
1100	    enqueue_replace_system_prompt_override(
1101	        &tx,
1102	        serde_json::json!({ "systemPromptOverride": "" }).as_object(),
1103	        None,
1104	    );
1105	    enqueue_replace_system_prompt_override(&tx, serde_json::json!({}).as_object(), None);
1106	    enqueue_replace_system_prompt_override(&tx, None, None);
1107	    assert!(
1108	        rx.try_recv().is_err(),
1109	        "no command should be enqueued without a non-empty override"
1110	    );
1111	}
1112	/// Regression for the web-client flow where `_meta.agentProfile` drives `set_session_model`.
1113	/// A zero-turn switch from `grok-build` (a client profile name) to `grok-build-plan` (the default model agent_type) must be treated as compatible.
1114	/// Compatible means the harness rebuild is skipped and the custom prompt body preserved.
1115	#[test]
1116	fn harnesses_are_compatible_for_stock_family_pairs() {
1117	    assert!(harnesses_are_compatible("grok-build", "grok-build-plan"));
1118	    assert!(harnesses_are_compatible("grok-build-plan", "grok-build"));
1119	    assert!(harnesses_are_compatible("grok-build", "grok-build"));
1120	    assert!(harnesses_are_compatible(
1121	        "grok-build-concise",
1122	        "grok-build-plan"
1123	    ));
1124	    assert!(harnesses_are_compatible(
1125	        "remote-sidebar",
1126	        "grok-build-plan"
1127	    ));
1128	}
1129	#[test]
1130	fn harnesses_are_compatible_rejects_strict_mismatches() {
1131	    assert!(harnesses_are_compatible("codex", "codex"));
1132	    assert!(!harnesses_are_compatible("grok-build-plan", "codex"));
1133	}
1134	#[test]
1135	fn explicit_agent_type_wins_over_session_default() {
1136	    assert_eq!(
1137	        resolve_required_agent_type(Some("cursor"), "grok-build-plan"),
1138	        "cursor"
1139	    );
1140	}
1141	#[test]
1142	fn null_agent_type_falls_back_to_session_default_grok_build_plan() {
1143	    assert_eq!(
1144	        resolve_required_agent_type(None, "grok-build-plan"),
1145	        "grok-build-plan"
1146	    );
1147	}
1148	#[test]
1149	fn null_agent_type_falls_back_to_session_default_grok_build() {
1150	    assert_eq!(
1151	        resolve_required_agent_type(None, "grok-build"),
1152	        "grok-build"
1153	    );
1154	}
1155	#[test]
1156	fn null_agent_type_returns_to_session_default_after_cursor_switch() {
1157	    let session_default = "grok-build-plan";
1158	    let required_after_null = resolve_required_agent_type(None, session_default);
1159	    assert_eq!(required_after_null, "grok-build-plan");
1160	    assert_ne!(required_after_null, "cursor");
1161	}
1162	/// Compatible stock switches (no rebuild) must NOT mutate `agent_name`, preserving the session's original ACP `agentProfile`.
1163	#[test]
1164	fn agent_name_unchanged_without_harness_rebuild() {
1165	    let unchanged = agent_name_after_model_switch(false, "grok-build-plan", "remote-sidebar");
1166	    assert_eq!(
1167	        unchanged, "remote-sidebar",
1168	        "a compatible stock switch must preserve the original agent profile name"
1169	    );
1170	}
1171	/// End-to-end test: config through resolve, override, and finalize to tool_definitions.
1172	/// The hashline tools must appear in the actual tool definitions that would be sent to the model.
1173	#[tokio::test]
1174	async fn file_toolset_override_e2e_to_finalized_toolset() {
1175	    use crate::tools::{FileToolset, ShellToolsetConfig};
1176	    use xai_grok_tools::computer::local::{LocalFs, LocalTerminalBackend};
1177	    use xai_grok_tools::notification::ToolNotificationHandle;
1178	    use xai_grok_tools::registry::types::SessionContext;
1179	    let tmp = tempfile::tempdir().unwrap();
1180	    let mut def = MvpAgent::resolve_agent_definition(
1181	        tmp.path(),
1182	        None,
1183	        &config::AgentSelectionConfig::default(),
1184	        None,
1185	        None,
1186	    );
1187	    let toolset_config = ShellToolsetConfig {
1188	        file_toolset: FileToolset::Hashline,
1189	        ..ShellToolsetConfig::default()
1190	    };
1191	    let effective = toolset_config.resolve_file_toolset(None);
1192	    let file_tools = effective
1193	        .tool_configs(&toolset_config.hashline)
1194	        .expect("default hashline config should validate");
1195	    def.override_file_tools(file_tools);
1196	    let builder = xai_grok_tools::registry::types::ToolRegistryBuilder::new();
1197	    let ctx = SessionContext {
1198	        backend: std::sync::Arc::new(LocalTerminalBackend::new()),
1199	        fs: std::sync::Arc::new(LocalFs),
1200	        cwd: tmp.path().to_path_buf(),
1201	        session_folder: tmp.path().join("session"),
1202	        session_env: std::sync::Arc::new(std::collections::HashMap::new()),
1203	        notification_handle: ToolNotificationHandle::noop(),
1204	        owner_session_id: None,
1205	        subagent: None,
1206	        parent_scheduler_handle: None,
1207	        skills: vec![],
1208	        state_path: tmp.path().join("state.json"),
1209	        memory_backend: None,
1210	        web_search_config: xai_grok_tools::implementations::web_search::WebSearchConfig::default(),
1211	        web_fetch_config: Default::default(),
1212	        lsp: None,
1213	        image_gen_config: xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig::default(),
1214	        video_gen_config: xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig::default(),
1215	        app_builder_deployer_config: xai_grok_tools::implementations::grok_build::app_builder::AppBuilderDeployerConfig::default(),
1216	        api_key_provider: None,
1217	        auth_provider: None,
1218	        attribution_callback: None,
1219	        system_reminder_tag: xai_grok_tools::reminders::DEFAULT_REMINDER_TAG,
1220	    };
1221	    let toolset = builder
1222	        .finalize(def.tool_config, ctx)
1223	        .expect("hashline toolset should finalize");
1224	    let defs = toolset.tool_definitions();
1225	    let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
1226	    assert!(names.contains(&"hashline_read"), "defs: {names:?}");
1227	    assert!(names.contains(&"hashline_edit"), "defs: {names:?}");
1228	    assert!(names.contains(&"hashline_grep"), "defs: {names:?}");
1229	    // The search slot holds both `grep` and the index search; the swap moves
1230	    // `grep` and leaves `search_code` in place with its hashline note on.
1231	    assert!(names.contains(&"search_code"), "defs: {names:?}");
1232	    assert!(!names.contains(&"grep"), "defs: {names:?}");
1233	    assert!(!names.contains(&"read_file"), "defs: {names:?}");
1234	    assert!(!names.contains(&"search_replace"), "defs: {names:?}");
1235	    assert!(names.contains(&"list_dir"), "defs: {names:?}");
1236	}
1237	/// Invalid hashline config returns a clean error, not a panic.
1238	#[test]
1239	fn file_toolset_override_invalid_config_returns_error() {
1240	    use crate::tools::FileToolset;
1241	    use crate::tools::config::HashlineSchemeConfig;
1242	    let bad = HashlineSchemeConfig {
1243	        scheme: "bogus".to_owned(),
1244	        hash_len: 0,
1245	        chunk_size: 0,
1246	    };
1247	    let err = FileToolset::Hashline.tool_configs(&bad);
1248	    assert!(err.is_err());
1249	    assert!(err.unwrap_err().contains("unknown"));
1250	}
1251	/// Requires a tokio runtime for SessionSignalsHandle::new().
1252	pub(super) fn make_test_handle(
1253	    model: &str,
1254	    yolo: bool,
1255	    client_id: Option<&str>,
1256	) -> crate::session::SessionHandle {
1257	    let (cmd_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
1258	    let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
1259	    let (hunk_event_tx, _hunk_event_rx) = tokio::sync::mpsc::unbounded_channel();
1260	    let hunk_cancel = tokio_util::sync::CancellationToken::new();
1261	    let hunk_tracker_handle = xai_hunk_tracker::HunkTrackerActor::spawn(
1262	        "test".to_string(),
1263	        std::env::temp_dir(),
1264	        hunk_event_tx,
1265	        xai_hunk_tracker::TrackingMode::AllDirty,
1266	        hunk_cancel,
1267	    );
1268	    crate::session::SessionHandle {
1269	        context_window_selection: Default::default(),
1270	        cmd_tx,
1271	        persistence_tx,
1272	        cancel_slot: Default::default(),
1273	        registry_write_order: Default::default(),
1274	        current_prompt_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
1275	        pending_interactions: std::sync::Arc::new(std::sync::Mutex::new(
1276	            std::collections::HashMap::new(),
1277	        )),
1278	        active_work: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
1279	        info: crate::session::info::Info {
1280	            id: acp::SessionId::new("test"),
1281	            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
1282	        },
1283	        max_turns: None,
1284	        resolved_tool_overrides: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
1285	        spawn_snapshot: crate::session::SpawnSnapshot {
1286	            applied_tool_overrides: None,
1287	            memory_mode: None,
1288	        },
1289	        hunk_tracker_handle,
1290	        chat_state_handle: xai_chat_state::ChatStateHandle::noop(),
1291	        signals_handle: crate::session::signals::SessionSignalsHandle::new(),
1292	        gateway_enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
1293	        emit_local_background_tasks: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
1294	        client_caps: crate::session::notifications::SessionClientCaps::new(false, true),
1295	        mcp_servers: Default::default(),
1296	        initial_client_mcp_servers: Default::default(),
1297	        display_cwd: None,
1298	        feedback_manager: std::sync::Arc::new(
1299	            crate::session::feedback_manager::FeedbackManager::local_only("test"),
1300	        ),
1301	        upload_queue: Arc::new(OnceLock::new()),
1302	        upload_failures_since_success: Arc::new(std::sync::atomic::AtomicU64::new(0)),
1303	        tool_context: crate::tools::ToolContext::new_local_context(
1304	            xai_grok_paths::AbsPathBuf::new(std::env::temp_dir()).unwrap(),
1305	            std::sync::Arc::new(xai_grok_workspace::file_system::LocalFs::new(
1306	                std::env::temp_dir(),
1307	            )),
1308	            std::sync::Arc::new(crate::terminal::LocalTerminalRunner),
1309	        ),
1310	        model_id: acp::ModelId::new(model),
1311	        reasoning_effort: None,
1312	        yolo_mode: yolo,
1313	        origin_client: client_id.map(|s| crate::http::OriginClientInfo {
1314	            product: s.to_string(),
1315	            version: None,
1316	        }),
1317	        code_nav_enabled: false,
1318	        ask_user_question_enabled: true,
1319	        non_interactive: false,
1320	        plan_mode: std::sync::Arc::new(parking_lot::Mutex::new(
1321	            crate::session::plan_mode::PlanModeTracker::new(std::env::temp_dir()),
1322	        )),
1323	        permission_handle: xai_grok_workspace::permission::PermissionHandle::allow_all(),
1324	        attribution_callback: None,
1325	        agent_name: "grok-build".to_string(),
1326	        managed_mcp_proxy_base_url: String::new(),
1327	        session_default_agent_profile: None,
1328	        allowed_subagent_types: None,
1329	        hook_registry: None,
1330	        workspace_ops: xai_grok_workspace::WorkspaceOps::for_test(),
1331	        terminal_backend: None,
1332	        tools_notification_handle: None,
1333	        scheduler_handle: None,
1334	    }
1335	}
1336	#[tokio::test]
1337	async fn lookup_session_model_returns_per_session_model() {
1338	    let default_model = acp::ModelId::new("default-model");
1339	    assert_eq!(
1340	        lookup_session_model(Some(acp::ModelId::new("grok-3-fast")), &default_model)
1341	            .0
1342	            .as_ref(),
1343	        "grok-3-fast"
1344	    );
1345	    assert_eq!(
1346	        lookup_session_model(Some(acp::ModelId::new("codex-mini")), &default_model)
1347	            .0
1348	            .as_ref(),
1349	        "codex-mini"
1350	    );
1351	}
1352	#[tokio::test]
1353	async fn lookup_session_model_fallback_no_session() {
1354	    let default_model = acp::ModelId::new("grok-3");
1355	    assert_eq!(
1356	        lookup_session_model(None, &default_model).0.as_ref(),
1357	        "grok-3"
1358	    );
1359	}
1360	/// Mutating session A's model_id via the handle does not affect session B.
1361	#[tokio::test]
1362	async fn set_session_model_does_not_cross_contaminate() {
1363	    let sid_a = acp::SessionId::new("sess-a");
1364	    let sid_b = acp::SessionId::new("sess-b");
1365	    let default_model = acp::ModelId::new("default");
1366	    let mut sessions: HashMap<acp::SessionId, crate::session::SessionHandle> = [
1367	        (sid_a.clone(), make_test_handle("grok-3", false, None)),
1368	        (sid_b.clone(), make_test_handle("grok-3", false, None)),
1369	    ]
1370	    .into();
1371	    sessions.get_mut(&sid_a).unwrap().model_id = acp::ModelId::new("codex-mini");
1372	    assert_eq!(
1373	        lookup_session_model(
1374	            sessions.get(&sid_a).map(|h| h.model_id.clone()),
1375	            &default_model
1376	        )
1377	        .0
1378	        .as_ref(),
1379	        "codex-mini"
1380	    );
1381	    assert_eq!(
1382	        lookup_session_model(
1383	            sessions.get(&sid_b).map(|h| h.model_id.clone()),
1384	            &default_model
1385	        )
1386	        .0
1387	        .as_ref(),
1388	        "grok-3",
1389	        "Session B's model must not be affected by session A's model change"
1390	    );
1391	}
1392	#[tokio::test]
1393	async fn model_state_prefers_session_reasoning_effort_over_global() {
1394	    use crate::agent::config::{EndpointsConfig, ModelEntry};
1395	    use xai_grok_sampling_types::{REASONING_EFFORT_META_KEY, ReasoningEffort};
1396	    let agent = build_minimal_agent_for_tests();
1397	    let mut entry = ModelEntry::fallback("effort-model", &EndpointsConfig::default());
1398	    entry.info.supports_reasoning_effort = true;
1399	    agent
1400	        .models_manager
1401	        .insert_test_entry("effort-model", entry);
1402	    agent
1403	        .models_manager
1404	        .set_current_reasoning_effort(Some(ReasoningEffort::Low));
1405	    let read_effort = |state: &acp::SessionModelState| -> Option<String> {
1406	        state
1407	            .available_models
1408	            .iter()
1409	            .find(|m| m.model_id.0.as_ref() == "effort-model")
1410	            .and_then(|m| m.meta.as_ref())
1411	            .and_then(|m| m.get(REASONING_EFFORT_META_KEY))
1412	            .and_then(|v| v.as_str())
1413	            .map(str::to_owned)
1414	    };
1415	    let pinned = acp::SessionId::new("sess-pinned");
1416	    let mut handle = make_test_handle("effort-model", false, None);
1417	    handle.reasoning_effort = Some(ReasoningEffort::Xhigh);
1418	    agent.insert_resident(&pinned, handle);
1419	    assert_eq!(
1420	        read_effort(&agent.model_state(Some(&pinned))).as_deref(),
1421	        Some("xhigh"),
1422	        "model_state must report the session's own restored effort",
1423	    );
1424	    let unset = acp::SessionId::new("sess-unset");
1425	    agent.insert_resident(&unset, make_test_handle("effort-model", false, None));
1426	    assert_eq!(
1427	        read_effort(&agent.model_state(Some(&unset))).as_deref(),
1428	        Some("low"),
1429	        "absent session effort falls back to the global default",
1430	    );
1431	}
1432	#[tokio::test]
1433	async fn apply_supported_effort_assigns_only_when_supported() {
1434	    use crate::agent::config::{EndpointsConfig, ModelEntry};
1435	    use crate::sampling::EffortTarget;
1436	    use xai_grok_sampling_types::ReasoningEffort;
1437	    let agent = build_minimal_agent_for_tests();
1438	    let mut supported = ModelEntry::fallback("effort-model", &EndpointsConfig::default());
1439	    supported.info.supports_reasoning_effort = true;
1440	    agent
1441	        .models_manager
1442	        .insert_test_entry("effort-model", supported.clone());
1443	    let plain = ModelEntry::fallback("plain-model", &EndpointsConfig::default());
1444	    agent
1445	        .models_manager
1446	        .insert_test_entry("plain-model", plain.clone());
1447	    let sid = acp::SessionId::new("meta-effort-sess");
1448	    let mut supported_cfg = agent.prepare_sampling_config_for_model(&supported, None);
1449	    supported_cfg.reasoning_effort = None;
1450	    agent.models_manager.apply_supported_effort(
1451	        &mut supported_cfg,
1452	        Some(ReasoningEffort::High),
1453	        &sid,
1454	        EffortTarget::NewSession,
1455	    );
1456	    assert_eq!(supported_cfg.reasoning_effort, Some(ReasoningEffort::High));
1457	    let mut plain_cfg = agent.prepare_sampling_config_for_model(&plain, None);
1458	    plain_cfg.reasoning_effort = None;
1459	    agent.models_manager.apply_supported_effort(
1460	        &mut plain_cfg,
1461	        Some(ReasoningEffort::High),
1462	        &sid,
1463	        EffortTarget::NewSession,
1464	    );
1465	    assert_eq!(plain_cfg.reasoning_effort, None);
1466	    let mut none_cfg = agent.prepare_sampling_config_for_model(&supported, None);
1467	    none_cfg.reasoning_effort = Some(ReasoningEffort::Low);
1468	    agent.models_manager.apply_supported_effort(
1469	        &mut none_cfg,
1470	        None,
1471	        &sid,
1472	        EffortTarget::NewSession,
1473	    );
1474	    assert_eq!(none_cfg.reasoning_effort, Some(ReasoningEffort::Low));
1475	}
1476	/// Setting `reasoning_effort` without switching the id runs, and bills, whatever the entry opened on.
1477	/// The status line would still read `low`.
1478	#[tokio::test]
1479	async fn an_effort_that_names_its_own_model_switches_the_id() {
1480	    use crate::agent::config::{EndpointsConfig, ModelEntry, ModelVariant};
1481	    use crate::sampling::EffortTarget;
1482	    use xai_grok_sampling_types::{ReasoningEffort, ReasoningEffortOption};
1483	    let agent = build_minimal_agent_for_tests();
1484	    let option = |value: ReasoningEffort, default: bool| ReasoningEffortOption {
1485	        id: value.as_ref().to_string(),
1486	        value,
1487	        label: value.as_ref().to_string(),
1488	        description: None,
1489	        default,
1490	    };
1491	    let variant = |effort: ReasoningEffort, model: &str| ModelVariant {
1492	        effort,
1493	        model_id: model.to_string(),
1494	    };
1495	    let mut routed = ModelEntry::fallback("routed-high", &EndpointsConfig::default());
1496	    routed.info.supports_reasoning_effort = true;
1497	    routed.info.reasoning_efforts = vec![
1498	        option(ReasoningEffort::Low, false),
1499	        option(ReasoningEffort::High, true),
1500	    ];
1501	    routed.info.variants = vec![
1502	        variant(ReasoningEffort::Low, "routed-low"),
1503	        variant(ReasoningEffort::High, "routed-high"),
1504	    ];
1505	    agent
1506	        .models_manager
1507	        .insert_test_entry("routed-high", routed.clone());
1508	    let mut plain = ModelEntry::fallback("plain-model", &EndpointsConfig::default());
1509	    plain.info.supports_reasoning_effort = true;
1510	    plain.info.reasoning_efforts = vec![
1511	        option(ReasoningEffort::Low, false),
1512	        option(ReasoningEffort::High, true),
1513	    ];
1514	    agent
1515	        .models_manager
1516	        .insert_test_entry("plain-model", plain.clone());
1517	    let sid = acp::SessionId::new("effort-routing-sess");
1518	    let mut cfg = agent.prepare_sampling_config_for_model(&routed, None);
1519	    assert_eq!(cfg.model, "routed-high");
1520	    agent.models_manager.apply_supported_effort(
1521	        &mut cfg,
1522	        Some(ReasoningEffort::Low),
1523	        &sid,
1524	        EffortTarget::ModelSwitch,
1525	    );
1526	    assert_eq!(cfg.model, "routed-low");
1527	    assert_eq!(cfg.reasoning_effort, Some(ReasoningEffort::Low));
1528	    let mut plain_cfg = agent.prepare_sampling_config_for_model(&plain, None);
1529	    agent.models_manager.apply_supported_effort(
1530	        &mut plain_cfg,
1531	        Some(ReasoningEffort::Low),
1532	        &sid,
1533	        EffortTarget::ModelSwitch,
1534	    );
1535	    assert_eq!(
1536	        plain_cfg.model, "plain-model",
1537	        "no id, so the model is left alone"
1538	    );
1539	    assert_eq!(plain_cfg.reasoning_effort, Some(ReasoningEffort::Low));
1540	}
1541	/// A session persists whichever id the effort picked, and the catalog has to find the entry from it.
1542	/// Otherwise it resumes on an unrelated model, with its retry, timeout and compaction settings back at the defaults.
1543	#[tokio::test]
1544	async fn a_routed_effort_id_resolves_back_to_its_entry() {
1545	    use crate::agent::config::{EndpointsConfig, ModelEntry, ModelVariant};
1546	    use xai_grok_sampling_types::{ReasoningEffort, ReasoningEffortOption};
1547	    let agent = build_minimal_agent_for_tests();
1548	    let mut entry = ModelEntry::fallback("routed-high", &EndpointsConfig::default());
1549	    entry.info.supports_reasoning_effort = true;
1550	    entry.info.reasoning_efforts = vec![ReasoningEffortOption {
1551	        id: "low".to_string(),
1552	        value: ReasoningEffort::Low,
1553	        label: "Low".to_string(),
1554	        description: None,
1555	        default: false,
1556	    }];
1557	    entry.info.variants = vec![ModelVariant {
1558	        effort: ReasoningEffort::Low,
1559	        model_id: "routed-low".to_string(),
1560	    }];
1561	    agent
1562	        .models_manager
1563	        .insert_test_entry("catalog-key", entry.clone());
1564	    assert_eq!(entry.info.model, "routed-high");
1565	    assert_eq!(entry.info.model_at(ReasoningEffort::High), "routed-high");
1566	    assert!(
1567	        agent
1568	            .models_manager
1569	            .model_supports_reasoning_effort("routed-low"),
1570	        "the id an effort routes to must resolve back to its entry",
1571	    );
1572	    assert_eq!(
1573	        agent
1574	            .models_manager
1575	            .model_for_effort("routed-low", ReasoningEffort::Low),
1576	        Some("routed-low".to_string()),
1577	        "so a second application is idempotent rather than losing the entry",
1578	    );
1579	}
1580	#[test]
1581	fn resolve_new_session_effort_hint_prefers_meta_over_current() {
1582	    use crate::agent::mvp_agent::reasoning_effort::resolve_new_session_effort_hint;
1583	    use xai_grok_sampling_types::ReasoningEffort;
1584	    assert_eq!(
1585	        resolve_new_session_effort_hint(Some(ReasoningEffort::High), Some(ReasoningEffort::Low)),
1586	        Some(ReasoningEffort::High),
1587	    );
1588	    assert_eq!(
1589	        resolve_new_session_effort_hint(None, Some(ReasoningEffort::Low)),
1590	        Some(ReasoningEffort::Low),
1591	        "/new and /clear with no _meta hint must keep last-used / config effort",
1592	    );
1593	    assert_eq!(resolve_new_session_effort_hint(None, None), None);
1594	}
1595	#[test]
1596	fn split_new_session_effort_routes_hint_to_one_slot() {
1597	    use crate::agent::mvp_agent::reasoning_effort::{NewSessionEffort, split_new_session_effort};
1598	    use xai_grok_sampling_types::ReasoningEffort;
1599	    assert_eq!(
1600	        split_new_session_effort(None, Some(ReasoningEffort::High)),
1601	        NewSessionEffort::Spawn(ReasoningEffort::High),
1602	    );
1603	    assert_eq!(
1604	        split_new_session_effort(Some("custom-model"), Some(ReasoningEffort::High)),
1605	        NewSessionEffort::Switch(ReasoningEffort::High),
1606	    );
1607	    assert_eq!(split_new_session_effort(None, None), NewSessionEffort::None,);
1608	    assert_eq!(
1609	        split_new_session_effort(Some("custom-model"), None),
1610	        NewSessionEffort::None,
1611	    );
1612	}
1613	/// New-session default-model path, composing `parse_reasoning_effort_meta`, `split_new_session_effort`, and `apply_supported_effort`.
1614	/// The chain seeds a `_meta.reasoningEffort` hint into the spawn sampling for a supported model.
1615	/// It drops the hint (keeping the catalog default) for an unsupported one.
1616	#[tokio::test]
1617	async fn new_session_meta_effort_seeds_spawn_for_supported_model_and_drops_for_unsupported() {
1618	    use crate::agent::config::{EndpointsConfig, ModelEntry};
1619	    use crate::agent::mvp_agent::reasoning_effort::{NewSessionEffort, split_new_session_effort};
1620	    use crate::sampling::EffortTarget;
1621	    use xai_grok_sampling_types::{
1622	        REASONING_EFFORT_META_KEY, ReasoningEffort, parse_reasoning_effort_meta,
1623	        reasoning_effort_meta_value,
1624	    };
1625	    let agent = build_minimal_agent_for_tests();
1626	    let mut supported = ModelEntry::fallback("effort-model", &EndpointsConfig::default());
1627	    supported.info.supports_reasoning_effort = true;
1628	    supported.info.reasoning_effort = Some(ReasoningEffort::Low);
1629	    agent
1630	        .models_manager
1631	        .insert_test_entry("effort-model", supported.clone());
1632	    let plain = ModelEntry::fallback("plain-model", &EndpointsConfig::default());
1633	    agent
1634	        .models_manager
1635	        .insert_test_entry("plain-model", plain.clone());
1636	    let mut meta = acp::Meta::new();
1637	    meta.insert(
1638	        REASONING_EFFORT_META_KEY.to_string(),
1639	        reasoning_effort_meta_value(ReasoningEffort::High),
1640	    );
1641	    let request = acp::NewSessionRequest::new(std::env::temp_dir()).meta(Some(meta));
1642	    let route = split_new_session_effort(None, parse_reasoning_effort_meta(request.meta.as_ref()));
1643	    assert_eq!(route, NewSessionEffort::Spawn(ReasoningEffort::High));
1644	    let spawn_effort = match route {
1645	        NewSessionEffort::Spawn(effort) => Some(effort),
1646	        NewSessionEffort::Switch(_) | NewSessionEffort::None => None,
1647	    };
1648	    let sid = acp::SessionId::new("new-session-meta-effort");
1649	    let mut supported_cfg = agent.prepare_sampling_config_for_model(&supported, None);
1650	    agent.models_manager.apply_supported_effort(
1651	        &mut supported_cfg,
1652	        spawn_effort,
1653	        &sid,
1654	        EffortTarget::NewSession,
1655	    );
1656	    assert_eq!(supported_cfg.reasoning_effort, Some(ReasoningEffort::High));
1657	    let mut plain_cfg = agent.prepare_sampling_config_for_model(&plain, None);
1658	    agent.models_manager.apply_supported_effort(
1659	        &mut plain_cfg,
1660	        spawn_effort,
1661	        &sid,
1662	        EffortTarget::NewSession,
1663	    );
1664	    assert_eq!(plain_cfg.reasoning_effort, None);
1665	}
1666	/// `/new` / `/clear` send no `_meta.reasoningEffort`.
1667	/// The last-used / config default must seed spawn so a fresh chat does not snap back to the catalog default (`high` on grok-4.6).
1668	#[tokio::test]
1669	async fn new_session_without_meta_keeps_current_effort_over_catalog_default() {
1670	    use crate::agent::config::{EndpointsConfig, ModelEntry};
1671	    use crate::agent::mvp_agent::reasoning_effort::{
1672	        NewSessionEffort, resolve_new_session_effort_hint, split_new_session_effort,
1673	    };
1674	    use crate::sampling::EffortTarget;
1675	    use xai_grok_sampling_types::ReasoningEffort;
1676	    let agent = build_minimal_agent_for_tests();
1677	    let mut supported = ModelEntry::fallback("effort-model", &EndpointsConfig::default());
1678	    supported.info.supports_reasoning_effort = true;
1679	    supported.info.reasoning_effort = Some(ReasoningEffort::High);
1680	    agent
1681	        .models_manager
1682	        .insert_test_entry("effort-model", supported.clone());
1683	    agent
1684	        .models_manager
1685	        .set_current_reasoning_effort(Some(ReasoningEffort::Low));
1686	    let hint =
1687	        resolve_new_session_effort_hint(None, agent.models_manager.current_reasoning_effort());
1688	    let route = split_new_session_effort(None, hint);
1689	    assert_eq!(route, NewSessionEffort::Spawn(ReasoningEffort::Low));
1690	    let spawn_effort = match route {
1691	        NewSessionEffort::Spawn(effort) => Some(effort),
1692	        NewSessionEffort::Switch(_) | NewSessionEffort::None => None,
1693	    };
1694	    let mut cfg = agent.prepare_sampling_config_for_model(&supported, None);
1695	    assert_eq!(cfg.reasoning_effort, Some(ReasoningEffort::High));
1696	    agent.models_manager.apply_supported_effort(
1697	        &mut cfg,
1698	        spawn_effort,
1699	        &acp::SessionId::new("new-session-current-effort"),
1700	        EffortTarget::NewSession,
1701	    );
1702	    assert_eq!(
1703	        cfg.reasoning_effort,
1704	        Some(ReasoningEffort::Low),
1705	        "/clear must keep last-used / config effort, not the catalog default",
1706	    );
1707	}
1708	/// Drive the real `restore_persisted_model` for a session pinned to an effort-capable model and report the effort it lands on the session handle.
1709	async fn restore_effort_via_load(
1710	    initial: Option<xai_grok_sampling_types::ReasoningEffort>,
1711	    persisted: Option<xai_grok_sampling_types::ReasoningEffort>,
1712	) -> Option<xai_grok_sampling_types::ReasoningEffort> {
1713	    use crate::agent::config::{EndpointsConfig, ModelEntry};
1714	    let agent = build_minimal_agent_for_tests();
1715	    let mut entry = ModelEntry::fallback("effort-model", &EndpointsConfig::default());
1716	    entry.info.supports_reasoning_effort = true;
1717	    agent
1718	        .models_manager
1719	        .insert_test_entry("effort-model", entry);
1720	    let sid = acp::SessionId::new("restore-precedence-sess");
1721	    let (handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
1722	    tokio::spawn(async move {
1723	        while let Some(cmd) = cmd_rx.recv().await {
1724	            if let crate::session::SessionCommand::SetSessionModel {
1725	                switch,
1726	                responds_to,
1727	            } = cmd
1728	            {
1729	                let _ = responds_to.send(Ok(acp::ModelId::new(switch.sampling_config.model)));
1730	            }
1731	        }
1732	    });
1733	    agent.insert_resident(&sid, handle);
1734	    let info = crate::session::info::Info {
1735	        id: sid.clone(),
1736	        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
1737	    };
1738	    let mut summary =
1739	        crate::session::persistence::Summary::new(&info, acp::ModelId::new("effort-model"))
1740	            .unwrap();
1741	    summary.reasoning_effort = persisted;
1742	    agent.restore_persisted_model(&sid, &summary, initial).await;
1743	    agent.resident_handle(&sid).and_then(|h| h.reasoning_effort)
1744	}
1745	#[tokio::test]
1746	async fn load_effort_precedence_prefers_meta_hint_over_persisted() {
1747	    use xai_grok_sampling_types::ReasoningEffort;
1748	    assert_eq!(
1749	        restore_effort_via_load(Some(ReasoningEffort::High), Some(ReasoningEffort::Low)).await,
1750	        Some(ReasoningEffort::High),
1751	    );
1752	    assert_eq!(
1753	        restore_effort_via_load(None, Some(ReasoningEffort::Low)).await,
1754	        Some(ReasoningEffort::Low),
1755	    );
1756	}
1757	#[tokio::test]
1758	async fn restore_applies_the_saved_context_window_selection() {
1759	    let agent = build_minimal_agent_for_tests();
1760	    let mut entry = ModelEntry::fallback("window-model", &EndpointsConfig::default());
1761	    entry.info.context_window = NonZeroU64::new(256_000).unwrap();
1762	    entry.info.context_windows = vec![
1763	        NonZeroU64::new(256_000).unwrap(),
1764	        NonZeroU64::new(500_000).unwrap(),
1765	    ];
1766	    agent
1767	        .models_manager
1768	        .insert_test_entry("window-model", entry);
1769	    agent.models_manager.settle_first_catalog_for_tests(true);
1770	    let sid = acp::SessionId::new("restore-window-sess");
1771	    let (handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
1772	    let (switch_tx, switch_rx) = tokio::sync::oneshot::channel();
1773	    tokio::spawn(async move {
1774	        while let Some(cmd) = cmd_rx.recv().await {
1775	            if let crate::session::SessionCommand::SetSessionModel {
1776	                switch,
1777	                responds_to,
1778	            } = cmd
1779	            {
1780	                let _ = switch_tx.send((
1781	                    switch.context_window_selection,
1782	                    switch
1783	                        .supported_context_windows
1784	                        .contains(&NonZeroU64::new(500_000).unwrap()),
1785	                ));
1786	                let _ = responds_to.send(Ok(acp::ModelId::new(switch.sampling_config.model)));
1787	                break;
1788	            }
1789	        }
1790	    });
1791	    agent.insert_resident(&sid, handle);
1792	    let info = crate::session::info::Info {
1793	        id: sid.clone(),
1794	        cwd: "/tmp".to_string(),
1795	    };
1796	    let mut summary =
1797	        crate::session::persistence::Summary::new(&info, acp::ModelId::new("window-model"))
1798	            .unwrap();
1799	    summary.context_window = NonZeroU64::new(500_000);
1800	    agent.restore_persisted_model(&sid, &summary, None).await;
1801	    assert_eq!(
1802	        (SwitchContextWindow::Set(NonZeroU64::new(500_000)), true),
1803	        switch_rx.await.expect("restore switches the model")
1804	    );
1805	}
1806	#[tokio::test]
1807	async fn restore_keeps_the_saved_context_window_selection_without_a_catalog() {
1808	    let agent = build_minimal_agent_for_tests();
1809	    let mut entry = ModelEntry::fallback("bundled-model", &EndpointsConfig::default());
1810	    entry.info.context_window = NonZeroU64::new(256_000).unwrap();
1811	    agent
1812	        .models_manager
1813	        .insert_test_entry("bundled-model", entry);
1814	    agent.models_manager.settle_first_catalog_for_tests(false);
1815	    let sid = acp::SessionId::new("restore-no-catalog-sess");
1816	    let (handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
1817	    let selection = handle.context_window_selection.clone();
1818	    let (switch_tx, switch_rx) = tokio::sync::oneshot::channel();
1819	    tokio::spawn(async move {
1820	        while let Some(cmd) = cmd_rx.recv().await {
1821	            if let crate::session::SessionCommand::SetSessionModel {
1822	                switch,
1823	                responds_to,
1824	            } = cmd
1825	            {
1826	                let _ = switch_tx.send((
1827	                    switch.context_window_selection,
1828	                    switch.sampling_config.context_window,
1829	                ));
1830	                let _ = responds_to.send(Ok(acp::ModelId::new(switch.sampling_config.model)));
1831	                break;
1832	            }
1833	        }
1834	    });
1835	    agent.insert_resident(&sid, handle);
1836	    let info = crate::session::info::Info {
1837	        id: sid.clone(),
1838	        cwd: "/tmp".to_string(),
1839	    };
1840	    let mut summary =
1841	        crate::session::persistence::Summary::new(&info, acp::ModelId::new("bundled-model"))
1842	            .unwrap();
1843	    summary.context_window = NonZeroU64::new(500_000);
1844	    agent.restore_persisted_model(&sid, &summary, None).await;
1845	    assert_eq!(
1846	        (SwitchContextWindow::Preserve, 256_000),
1847	        switch_rx.await.expect("restore switches the model")
1848	    );
1849	    assert_eq!(
1850	        500_000,
1851	        selection.load(std::sync::atomic::Ordering::Relaxed)
1852	    );
1853	}
1854	/// A session persisted under a routing *slug* (not the catalog map key) must still get reasoning modes and a selected model.
1855	/// `session_config_options` resolves the id to the catalog key before the catalog effort lookups and the selected-model match.
1856	#[tokio::test]
1857	async fn session_config_options_resolves_routing_slug_to_catalog_model() {
1858	    use crate::agent::config::{EndpointsConfig, ModelEntry};
1859	    use xai_grok_sampling_types::ReasoningEffort;
1860	    let agent = build_minimal_agent_for_tests();
1861	    let mut entry = ModelEntry::fallback("catalog-key-model", &EndpointsConfig::default());
1862	    entry.info.model = "routing-slug".to_string();
1863	    entry.info.supports_reasoning_effort = true;
1864	    entry.info.reasoning_effort = Some(ReasoningEffort::High);
1865	    agent
1866	        .models_manager
1867	        .insert_test_entry("catalog-key-model", entry);
1868	    let sid = acp::SessionId::new("sess-slug");
1869	    agent.insert_resident(&sid, make_test_handle("routing-slug", false, None));
1870	    let state = agent.model_state(Some(&sid));
1871	    assert_eq!(state.current_model_id.0.as_ref(), "routing-slug");
1872	    let opts = agent.session_config_options(Some(&sid), &state);
1873	    let modes: Vec<_> = opts.iter().filter(|o| o.category == "mode").collect();
1874	    assert!(
1875	        !modes.is_empty(),
1876	        "reasoning modes must surface for a slug-identified session"
1877	    );
1878	    assert!(
1879	        modes.iter().any(|o| o.id == "high" && o.selected),
1880	        "catalog default effort should be selected"
1881	    );
1882	    assert!(
1883	        opts.iter()
1884	            .any(|o| o.category == "model" && o.id == "catalog-key-model" && o.selected),
1885	        "resolved catalog model must be selected"
1886	    );
1887	}
1888	#[tokio::test]
1889	async fn yolo_toggle_scoped_by_client_identifier() {
1890	    let sid_tui = acp::SessionId::new("sess-tui");
1891	    let sid_vscode = acp::SessionId::new("sess-vscode");
1892	    let mut sessions: HashMap<acp::SessionId, crate::session::SessionHandle> = [
1893	        (
1894	            sid_tui.clone(),
1895	            make_test_handle("grok-3", false, Some("grok-tui")),
1896	        ),
1897	        (
1898	            sid_vscode.clone(),
1899	            make_test_handle("grok-3", false, Some("grok-code-extension")),
1900	        ),
1901	    ]
1902	    .into();
1903	    let updated =
1904	        apply_yolo_mode_to_matching_sessions(sessions.values_mut(), Some("grok-tui"), true);
1905	    assert_eq!(updated, 1, "exactly one matching session should be updated");
1906	    assert!(
1907	        sessions.get(&sid_tui).is_some_and(|s| s.yolo_mode),
1908	        "TUI session should have yolo=true after TUI toggle"
1909	    );
1910	    assert!(
1911	        sessions.get(&sid_vscode).is_some_and(|s| !s.yolo_mode),
1912	        "VS Code session must NOT be affected by TUI's yolo toggle"
1913	    );
1914	}
1915	/// A client can explicitly disable YOLO for its own sessions after startup, even if those sessions were initially created with yolo=true.
1916	#[tokio::test]
1917	async fn yolo_toggle_can_disable_session_started_with_yolo_enabled() {
1918	    let sid_tui = acp::SessionId::new("sess-tui");
1919	    let sid_other = acp::SessionId::new("sess-other");
1920	    let mut sessions: HashMap<acp::SessionId, crate::session::SessionHandle> = [
1921	        (
1922	            sid_tui.clone(),
1923	            make_test_handle("grok-3", true, Some("grok-tui")),
1924	        ),
1925	        (
1926	            sid_other.clone(),
1927	            make_test_handle("grok-3", true, Some("grok-code-extension")),
1928	        ),
1929	    ]
1930	    .into();
1931	    let updated =
1932	        apply_yolo_mode_to_matching_sessions(sessions.values_mut(), Some("grok-tui"), false);
1933	    assert_eq!(updated, 1, "only the sender's session should be updated");
1934	    assert!(
1935	        sessions.get(&sid_tui).is_some_and(|s| !s.yolo_mode),
1936	        "sender session should be switched to yolo=false"
1937	    );
1938	    assert!(
1939	        sessions.get(&sid_other).is_some_and(|s| s.yolo_mode),
1940	        "other client's session must keep its previous yolo state"
1941	    );
1942	}
1943	/// `drain_old_session_thread` returns immediately when the thread has already finished.
1944	#[tokio::test]
1945	async fn drain_finished_thread_returns_immediately() {
1946	    let session_threads: RefCell<HashMap<acp::SessionId, crate::session::SessionThread>> =
1947	        RefCell::new(HashMap::new());
1948	    let sid = acp::SessionId::new("drain-test");
1949	    let handle = std::thread::spawn(|| {});
1950	    std::thread::sleep(std::time::Duration::from_millis(10));
1951	    session_threads.borrow_mut().insert(
1952	        sid.clone(),
1953	        crate::session::SessionThread::from_handle(handle),
1954	    );
1955	    let thread = session_threads.borrow_mut().remove(&sid).unwrap();
1956	    assert!(thread.is_finished(), "thread should be finished");
1957	    assert!(!session_threads.borrow().contains_key(&sid));
1958	}
1959	/// `drain_old_session_thread` waits for a slow thread to finish.
1960	#[tokio::test]
1961	async fn drain_waits_for_slow_thread() {
1962	    let session_threads: RefCell<HashMap<acp::SessionId, crate::session::SessionThread>> =
1963	        RefCell::new(HashMap::new());
1964	    let sid = acp::SessionId::new("slow-drain");
1965	    let handle = std::thread::spawn(|| {
1966	        std::thread::sleep(std::time::Duration::from_millis(100));
1967	    });
1968	    session_threads.borrow_mut().insert(
1969	        sid.clone(),
1970	        crate::session::SessionThread::from_handle(handle),
1971	    );
1972	    let thread = session_threads.borrow_mut().remove(&sid).unwrap();
1973	    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
1974	    loop {
1975	        if thread.is_finished() {
1976	            break;
1977	        }
1978	        assert!(
1979	            tokio::time::Instant::now() < deadline,
1980	            "thread should finish within 5s"
1981	        );
1982	        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
1983	    }
1984	    assert!(thread.is_finished());
1985	}
1986	/// Drain respects the 5s deadline and returns even if the thread is still running.
1987	#[tokio::test]
1988	async fn drain_respects_deadline() {
1989	    let session_threads: RefCell<HashMap<acp::SessionId, crate::session::SessionThread>> =
1990	        RefCell::new(HashMap::new());
1991	    let sid = acp::SessionId::new("hung-drain");
1992	    let handle = std::thread::spawn(|| {
1993	        std::thread::sleep(std::time::Duration::from_secs(30));
1994	    });
1995	    session_threads.borrow_mut().insert(
1996	        sid.clone(),
1997	        crate::session::SessionThread::from_handle(handle),
1998	    );
1999	    let thread = session_threads.borrow_mut().remove(&sid).unwrap();
2000	    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(200);
2001	    let mut timed_out = false;
2002	    loop {
2003	        if thread.is_finished() {
2004	            break;
2005	        }
2006	        if tokio::time::Instant::now() >= deadline {
2007	            timed_out = true;
2008	            break;
2009	        }
2010	        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
2011	    }
2012	    assert!(
2013	        timed_out,
2014	        "should have timed out waiting for the hung thread"
2015	    );
2016	    assert!(!thread.is_finished(), "thread should still be running");
2017	}
2018	#[test]
2019	fn parse_code_nav_capability_present_and_true() {
2020	    let mut meta = serde_json::Map::new();
2021	    meta.insert(
2022	        "x.ai/codeNavigation".to_string(),
2023	        serde_json::json!({ "enabled": true }),
2024	    );
2025	    let init = acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
2026	        acp::ClientCapabilities::new()
2027	            .fs(acp::FileSystemCapabilities::new())
2028	            .terminal(false)
2029	            .meta(meta),
2030	    );
2031	    assert!(MvpAgent::parse_code_nav_capability(&init));
2032	}
2033	#[test]
2034	fn parse_code_nav_capability_absent_returns_false() {
2035	    let init = acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
2036	        acp::ClientCapabilities::new()
2037	            .fs(acp::FileSystemCapabilities::new())
2038	            .terminal(false),
2039	    );
2040	    assert!(!MvpAgent::parse_code_nav_capability(&init));
2041	}
2042	#[test]
2043	fn parse_code_nav_capability_false_returns_false() {
2044	    let mut meta = serde_json::Map::new();
2045	    meta.insert(
2046	        "x.ai/codeNavigation".to_string(),
2047	        serde_json::json!({ "enabled": false }),
2048	    );
2049	    let init = acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
2050	        acp::ClientCapabilities::new()
2051	            .fs(acp::FileSystemCapabilities::new())
2052	            .terminal(false)
2053	            .meta(meta),
2054	    );
2055	    assert!(!MvpAgent::parse_code_nav_capability(&init));
2056	}
2057	/// Verify that two session handles with different code-nav state produce independent eligibility outcomes, the key leader-mode isolation test.
2058	/// This tests the `code_nav_eligibility_for_request` lookup path directly by inspecting the per-handle fields rather than building a full agent.
2059	/// That mirrors what the method actually reads at runtime.
2060	#[tokio::test]
2061	async fn test_per_session_code_nav_isolation() {
2062	    let web_handle = {
2063	        let mut h = make_test_handle("model", false, Some("grok-web"));
2064	        h.code_nav_enabled = true;
2065	        h
2066	    };
2067	    let tui_handle = {
2068	        let mut h = make_test_handle("model", false, Some("grok-tui"));
2069	        h.code_nav_enabled = false;
2070	        h
2071	    };
2072	    let check = |handle: &crate::session::SessionHandle| {
2073	        let ct = crate::http::client_type_from_origin(handle.origin_client.as_ref());
2074	        if !matches!(ct, ClientType::GrokWeb) {
2075	            return Err(CodeNavEligibility::ClientNotWeb);
2076	        }
2077	        if !handle.code_nav_enabled {
2078	            return Err(CodeNavEligibility::CapabilityNotAdvertised);
2079	        }
2080	        Ok(())
2081	    };
2082	    assert!(
2083	        check(&web_handle).is_ok(),
2084	        "web session with capability should pass client-type and capability gates"
2085	    );
2086	    assert_eq!(
2087	        check(&tui_handle),
2088	        Err(CodeNavEligibility::ClientNotWeb),
2089	        "tui session should be rejected at gate 1"
2090	    );
2091	    let mut web_no_cap = web_handle.clone();
2092	    web_no_cap.code_nav_enabled = false;
2093	    assert_eq!(
2094	        check(&web_no_cap),
2095	        Err(CodeNavEligibility::CapabilityNotAdvertised),
2096	        "web session without capability should be rejected at gate 2"
2097	    );
2098	    assert!(
2099	        check(&web_handle).is_ok(),
2100	        "original web handle must be unaffected"
2101	    );
2102	}
2103	#[tokio::test(flavor = "current_thread")]
2104	async fn ext_method_routes_auth_cleared_and_refreshes_resident_sessions() {
2105	    let local = tokio::task::LocalSet::new();
2106	    local
2107	        .run_until(async {
2108	            let agent = build_agent_with_auth(xai_grok_login::GrokAuth {
2109	                key: "eligible".into(),
2110	                auth_mode: xai_grok_login::AuthMode::WebLogin,
2111	                ..xai_grok_login::GrokAuth::test_default()
2112	            });
2113	            use acp::Agent as _;
2114	            agent.managed_mcp_cache.lock().await.enable_gateway_tools();
2115	            let sid = acp::SessionId::new("sess-auth-cleared");
2116	            let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
2117	            agent.insert_resident(&sid, handle);
2118	            let params = serde_json::json!({});
2119	            agent
2120	                .ext_method(acp::ExtRequest::new(
2121	                    "x.ai/internal/auth_cleared",
2122	                    std::sync::Arc::from(serde_json::value::to_raw_value(&params).unwrap()),
2123	                ))
2124	                .await
2125	                .expect("auth_cleared must route through session-admin");
2126	            let cmd = tokio::time::timeout(std::time::Duration::from_secs(1), cmd_rx.recv())
2127	                .await
2128	                .expect("refresh command should be sent")
2129	                .expect("channel should stay open until command is received");
2130	            assert!(matches!(cmd, SessionCommand::RefreshMcpSearchIndex));
2131	            assert!(!agent.managed_mcp_cache.lock().await.gateway_tools_active);
2132	        })
2133	        .await;
2134	}
2135	fn empty_gateway_catalog() -> crate::session::managed_mcp::GatewayToolCatalog {
2136	    crate::session::managed_mcp::GatewayToolCatalog {
2137	        tools: vec![],
2138	        total_tools: 0,
2139	        connectors_needing_reauth: vec![],
2140	        reauth_connectors: vec![],
2141	    }
2142	}
2143	fn assert_no_update_mcp_servers(cmds: &[SessionCommand]) {
2144	    assert!(
2145	        !cmds
2146	            .iter()
2147	            .any(|c| matches!(c, SessionCommand::UpdateMcpServers { .. })),
2148	        "mcp/list must not push UpdateMcpServers"
2149	    );
2150	}
2151	/// `mcp/list` refresh with two resident sessions: cache=false with a committed catalog fans `RefreshMcpSearchIndex`.
2152	/// A failed catalog and cache=true do not.
2153	#[tokio::test(flavor = "current_thread")]
2154	async fn mcp_list_gateway_refresh_fans_only_on_committed_uncached_catalog() {
2155	    let local = tokio::task::LocalSet::new();
2156	    local
2157	        .run_until(async {
2158	            use crate::session::managed_mcp::GatewayToolCatalogCache;
2159	            use std::sync::atomic::{AtomicUsize, Ordering};
2160	            let list_hits = std::sync::Arc::new(AtomicUsize::new(0));
2161	            let list_hits_route = list_hits.clone();
2162	            let app = axum::Router::new().route(
2163	                "/mcp/tools/list",
2164	                axum::routing::get(move || {
2165	                    list_hits_route.fetch_add(1, Ordering::SeqCst);
2166	                    async {
2167	                        axum::Json(serde_json::json!({
2168	                            "tools": [{
2169	                                "connector_id": "gmail",
2170	                                "connector_name": "Gmail",
2171	                                "tool_id": "search",
2172	                                "tool_name": "Search",
2173	                                "call_id": "gmail_search",
2174	                                "description": "d",
2175	                                "json_schema": {"type": "object"}
2176	                            }],
2177	                            "total_tools": 1,
2178	                            "connectors_needing_reauth": []
2179	                        }))
2180	                    }
2181	                }),
2182	            );
2183	            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
2184	            let proxy_url = format!("http://{}", listener.local_addr().unwrap());
2185	            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
2186	            let (agent, _rx) = build_agent_with_auth_and_proxy(
2187	                xai_grok_login::GrokAuth {
2188	                    key: "eligible".into(),
2189	                    auth_mode: xai_grok_login::AuthMode::WebLogin,
2190	                    ..xai_grok_login::GrokAuth::test_default()
2191	                },
2192	                proxy_url,
2193	                crate::agent::config::AgentMode::Generic,
2194	            );
2195	            agent.cfg.borrow_mut().managed_mcp_gateway_tools_enabled = true;
2196	            let sid_a = acp::SessionId::new("sess-list-a");
2197	            let sid_b = acp::SessionId::new("sess-list-b");
2198	            let (handle_a, _tx_a, mut cmd_rx_a) = make_live_session_handle(&sid_a, None);
2199	            let (handle_b, _tx_b, mut cmd_rx_b) = make_live_session_handle(&sid_b, None);
2200	            agent.insert_resident(&sid_a, handle_a);
2201	            agent.insert_resident(&sid_b, handle_b);
2202	            {
2203	                let mut state = agent.managed_mcp_cache.lock().await;
2204	                state.enable_gateway_tools();
2205	                let epoch = state.start_gateway_tool_fetch().unwrap();
2206	                assert!(state.complete_gateway_tool_fetch(epoch, empty_gateway_catalog()));
2207	            }
2208	            let cached = agent.fetch_gateway_catalog_for_mcp_list(true).await;
2209	            let cached = cached.expect("cache=true should hit Ready catalog");
2210	            assert!(
2211	                cached.tools.is_empty() && cached.total_tools == 0,
2212	                "cache=true must return the seeded empty catalog, not a mock refetch"
2213	            );
2214	            assert_eq!(
2215	                list_hits.load(Ordering::SeqCst),
2216	                0,
2217	                "cache=true must not hit /mcp/tools/list"
2218	            );
2219	            assert!(
2220	                cmd_rx_a.try_recv().is_err() && cmd_rx_b.try_recv().is_err(),
2221	                "cache=true must not fan RefreshMcpSearchIndex"
2222	            );
2223	            match &agent.managed_mcp_cache.lock().await.gateway_tool_cache {
2224	                GatewayToolCatalogCache::Ready(catalog) => {
2225	                    assert!(
2226	                        catalog.tools.is_empty() && catalog.total_tools == 0,
2227	                        "in-memory cache must stay the seeded empty Ready catalog"
2228	                    );
2229	                }
2230	                GatewayToolCatalogCache::NotFetched => {
2231	                    panic!("expected Ready(empty), got NotFetched")
2232	                }
2233	                GatewayToolCatalogCache::Fetching(_) => {
2234	                    panic!("expected Ready(empty), got Fetching")
2235	                }
2236	            }
2237	            agent.cfg.borrow_mut().managed_mcp_gateway_tools_enabled = false;
2238	            let failed = agent.fetch_gateway_catalog_for_mcp_list(false).await;
2239	            assert!(failed.is_none(), "gateway off after invalidate is None");
2240	            assert!(
2241	                cmd_rx_a.try_recv().is_err() && cmd_rx_b.try_recv().is_err(),
2242	                "failed catalog must not fan RefreshMcpSearchIndex"
2243	            );
2244	            agent.cfg.borrow_mut().managed_mcp_gateway_tools_enabled = true;
2245	            let fresh = agent.fetch_gateway_catalog_for_mcp_list(false).await;
2246	            assert!(
2247	                fresh.is_some_and(|c| !c.tools.is_empty()),
2248	                "cache=false should refetch a non-empty catalog"
2249	            );
2250	            assert!(
2251	                list_hits.load(Ordering::SeqCst) >= 1,
2252	                "cache=false refetch must hit /mcp/tools/list"
2253	            );
2254	            let cmd_a = tokio::time::timeout(std::time::Duration::from_secs(1), cmd_rx_a.recv())
2255	                .await
2256	                .expect("session A RefreshMcpSearchIndex")
2257	                .expect("channel open");
2258	            let cmd_b = tokio::time::timeout(std::time::Duration::from_secs(1), cmd_rx_b.recv())
2259	                .await
2260	                .expect("session B RefreshMcpSearchIndex")
2261	                .expect("channel open");
2262	            assert!(matches!(cmd_a, SessionCommand::RefreshMcpSearchIndex));
2263	            assert!(matches!(cmd_b, SessionCommand::RefreshMcpSearchIndex));
2264	            assert_no_update_mcp_servers(&[cmd_a, cmd_b]);
2265	            assert!(cmd_rx_a.try_recv().is_err() && cmd_rx_b.try_recv().is_err());
2266	            server.abort();
2267	        })
2268	        .await;
2269	}
2270	/// mcp/list with gateway off must disable the cache, same as initialize.
2271	#[tokio::test(flavor = "current_thread")]
2272	async fn mcp_list_gateway_off_disables_cached_catalog() {
2273	    let local = tokio::task::LocalSet::new();
2274	    local
2275	        .run_until(async {
2276	            use crate::session::managed_mcp::GatewayToolCatalogCache;
2277	            let (agent, _rx) = build_agent_with_auth_and_proxy(
2278	                xai_grok_login::GrokAuth {
2279	                    key: "eligible".into(),
2280	                    auth_mode: xai_grok_login::AuthMode::WebLogin,
2281	                    ..xai_grok_login::GrokAuth::test_default()
2282	                },
2283	                "http://127.0.0.1:1".into(),
2284	                crate::agent::config::AgentMode::Generic,
2285	            );
2286	            agent.cfg.borrow_mut().managed_mcp_gateway_tools_enabled = true;
2287	            let sid = acp::SessionId::new("sess-list-off");
2288	            let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
2289	            agent.insert_resident(&sid, handle);
2290	            {
2291	                let mut state = agent.managed_mcp_cache.lock().await;
2292	                state.enable_gateway_tools();
2293	                let epoch = state.start_gateway_tool_fetch().unwrap();
2294	                assert!(state.complete_gateway_tool_fetch(epoch, empty_gateway_catalog()));
2295	            }
2296	            agent.cfg.borrow_mut().managed_mcp_gateway_tools_enabled = false;
2297	            assert!(
2298	                agent
2299	                    .fetch_gateway_catalog_for_mcp_list(true)
2300	                    .await
2301	                    .is_none(),
2302	                "gateway off must return None"
2303	            );
2304	            let state = agent.managed_mcp_cache.lock().await;
2305	            assert!(
2306	                !state.gateway_tools_active,
2307	                "gateway off must disable the cache"
2308	            );
2309	            assert!(
2310	                matches!(
2311	                    state.gateway_tool_cache,
2312	                    GatewayToolCatalogCache::NotFetched
2313	                ),
2314	                "disable must clear a Ready catalog"
2315	            );
2316	            drop(state);
2317	            assert!(
2318	                cmd_rx.try_recv().is_err(),
2319	                "disable via mcp/list must not fan RefreshMcpSearchIndex"
2320	            );
2321	        })
2322	        .await;
2323	}
2324	/// `/skills` scans disk for the modal and must also refresh every live session's baseline.
2325	#[tokio::test(flavor = "current_thread")]
2326	async fn skills_list_refreshes_session_skill_baseline() {
2327	    let agent = build_minimal_agent_for_tests();
2328	    let sid = acp::SessionId::new("sess-skills-list");
2329	    let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
2330	    agent.insert_resident(&sid, handle);
2331	    let req = acp::ExtRequest::new(
2332	        "x.ai/skills/list",
2333	        serde_json::value::to_raw_value(&serde_json::json!({ "cwd": "/tmp" }))
2334	            .unwrap()
2335	            .into(),
2336	    );
2337	    crate::extensions::skills::handle(
2338	        &agent,
2339	        &req,
2340	        None,
2341	        xai_grok_agent::prompt::skills::CompatConfig::default(),
2342	    )
2343	    .await
2344	    .expect("skills/list succeeds");
2345	    let cmd = tokio::time::timeout(std::time::Duration::from_secs(1), cmd_rx.recv())
2346	        .await
2347	        .expect("RefreshSkillBaseline should be sent")
2348	        .expect("channel should stay open");
2349	    assert!(matches!(cmd, SessionCommand::RefreshSkillBaseline));
2350	}
2351	/// Gateway tools live on the agent catalog, so sessions only rebuild `search_tool`.
2352	#[tokio::test(flavor = "current_thread")]
2353	async fn refresh_mcp_search_index_broadcasts_to_sessions() {
2354	    let agent = build_minimal_agent_for_tests();
2355	    let sid = acp::SessionId::new("sess-search-index");
2356	    let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
2357	    agent.insert_resident(&sid, handle);
2358	    agent.refresh_mcp_search_index_in_sessions();
2359	    let cmd = tokio::time::timeout(std::time::Duration::from_secs(1), cmd_rx.recv())
2360	        .await
2361	        .expect("RefreshMcpSearchIndex should be sent")
2362	        .expect("channel should stay open");
2363	    assert!(matches!(cmd, SessionCommand::RefreshMcpSearchIndex));
2364	}
2365	fn test_root_identity(agent: u128, attempt: u128) -> super::agent_directory::PendingRootIdentity {
2366	    super::agent_directory::PendingRootIdentity {
2367	        agent_id: xai_message_delivery_core::AgentId::mint(agent),
2368	        attempt_id: xai_message_delivery_core::AttemptId::mint(attempt),
2369	        origin: crate::agent::roster::RosterOrigin::Local,
2370	    }
2371	}
2372	fn assert_root_views(
2373	    agent: &MvpAgent,
2374	    sid: &acp::SessionId,
2375	    expected: Option<&super::agent_directory::RootDirectorySnapshot>,
2376	) {
2377	    assert_eq!(
2378	        expected.cloned(),
2379	        agent.session_registry.root_for_session(sid)
2380	    );
2381	    let live = agent.session_registry.snapshot_live_roots();
2382	    match expected {
2383	        Some(root) => {
2384	            assert_eq!(
2385	                Some(root.clone()),
2386	                agent.session_registry.root_for_agent(&root.agent_id)
2387	            );
2388	            assert!(live.contains(root));
2389	        }
2390	        None => assert!(live.iter().all(|root| root.session_id != *sid)),
2391	    }
2392	}
2393	fn build_minimal_agent_for_tests() -> MvpAgent {
2394	    use crate::agent::config::Config as AgentConfig;
2395	    use xai_grok_login::{AuthManager, GrokComConfig};
2396	    let temp_dir = tempfile::tempdir().unwrap();
2397	    let auth_manager =
2398	        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
2399	    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
2400	    let gateway = GatewaySender::new(tx);
2401	    let cfg = AgentConfig::default();
2402	    MvpAgent::new(gateway, &cfg, auth_manager, None, None).expect("valid test config")
2403	}
2404	fn session_usage_request(session_id: &str) -> acp::ExtRequest {
2405	    acp::ExtRequest::new(
2406	        "x.ai/session/usage",
2407	        serde_json::value::to_raw_value(&serde_json::json!({ "sessionId": session_id }))
2408	            .unwrap()
2409	            .into(),
2410	    )
2411	}
2412	#[tokio::test(flavor = "current_thread")]
2413	async fn session_usage_unknown_session_is_resource_not_found() {
2414	    let agent = build_minimal_agent_for_tests();
2415	    let err = crate::extensions::usage::handle(&agent, &session_usage_request("no-such-session"))
2416	        .await
2417	        .expect_err("unknown session");
2418	    assert_eq!(
2419	        err.code,
2420	        acp::Error::resource_not_found(None::<String>).code
2421	    );
2422	}
2423	#[tokio::test(flavor = "current_thread")]
2424	async fn session_usage_dead_chat_state_actor_fails_closed() {
2425	    let agent = build_minimal_agent_for_tests();
2426	    let sid = acp::SessionId::new("usage-dead-actor-sess");
2427	    let mut handle = make_test_handle("test-model", false, None);
2428	    handle.info.id = sid.clone();
2429	    agent.insert_resident(&sid, handle);
2430	    let err =
2431	        crate::extensions::usage::handle(&agent, &session_usage_request("usage-dead-actor-sess"))
2432	            .await
2433	            .expect_err("dead chat-state actor");
2434	    assert_eq!(err.code, acp::Error::internal_error().code);
2435	}
2436	/// Session responses publish the values this session's spawn pinned.
2437	#[tokio::test(flavor = "current_thread")]
2438	async fn session_meta_publishes_the_sessions_spawn_pins() {
2439	    let agent = build_minimal_agent_for_tests();
2440	    let sid = acp::SessionId::new("memory-mode-sess");
2441	    let mut handle = make_test_handle("test-model", false, None);
2442	    handle.info.id = sid.clone();
2443	    handle.spawn_snapshot.memory_mode = Some(crate::config::MemoryMode::V2);
2444	    agent.insert_resident(&sid, handle);
2445	    let model_state = agent.model_state(Some(&sid));
2446	    let mut meta = serde_json::Map::new();
2447	    agent.insert_session_config_meta(
2448	        &mut meta,
2449	        &sid,
2450	        std::env::temp_dir().to_string_lossy().into_owned(),
2451	        None,
2452	        &model_state,
2453	    );
2454	    assert_eq!(
2455	        meta.get(crate::session::MEMORY_MODE_META_KEY),
2456	        Some(&serde_json::json!("v2")),
2457	        "session meta must carry the handle's pinned memory mode"
2458	    );
2459	}
2460	fn build_agent_with_auth(auth: xai_grok_login::GrokAuth) -> MvpAgent {
2461	    use crate::agent::config::Config as AgentConfig;
2462	    use xai_grok_login::{AuthManager, GrokComConfig};
2463	    let temp_dir = tempfile::tempdir().unwrap();
2464	    let auth_manager =
2465	        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
2466	    auth_manager.hot_swap(auth);
2467	    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
2468	    let gateway = GatewaySender::new(tx);
2469	    let cfg = AgentConfig::default();
2470	    MvpAgent::new(gateway, &cfg, auth_manager, None, None).expect("valid test config")
2471	}
2479	fn personal_xai_oauth_auth() -> xai_grok_login::GrokAuth {
2480	    xai_grok_login::GrokAuth {
2481	        auth_mode: xai_grok_login::AuthMode::Oidc,
2482	        oidc_issuer: Some(xai_grok_login::XAI_OAUTH2_ISSUER.to_string()),
2483	        ..xai_grok_login::GrokAuth::test_default()
2484	    }
2485	}
3105	/// Regression: boot-time plugin discovery is deferred past ACP `initialize`, so the shared plugin registry starts empty. `resolve_mcp_servers` reads that snapshot to merge plugin-contributed MCP servers into a new session.
3106	/// Without lazy population the servers silently vanished until an explicit `/plugins reload`. `ensure_plugin_registry` must build the snapshot on first use.
3107	#[tokio::test]
3108	#[serial_test::serial]
3109	async fn ensure_plugin_registry_lazily_populates_snapshot() {
3110	    use crate::agent::config::Config as AgentConfig;
3111	    use xai_grok_login::{AuthManager, GrokComConfig};
3112	    use xai_grok_test_support::EnvGuard;
3113	    let grok_home = tempfile::tempdir().unwrap();
3114	    let _env = EnvGuard::set("GROK_HOME", grok_home.path());
3115	    let plugin_dir = tempfile::tempdir().unwrap();
3116	    std::fs::write(
3117	        plugin_dir.path().join("plugin.json"),
3118	        r#"{"name": "regr-lazy-mcp-plugin"}"#,
3119	    )
3120	    .unwrap();
3121	    std::fs::write(
3122	        plugin_dir.path().join(".mcp.json"),
3123	        r#"{"mcpServers":{"regr-srv":{"command":"echo","args":["hi"]}}}"#,
3124	    )
3125	    .unwrap();
3126	    let auth_home = tempfile::tempdir().unwrap();
3127	    let auth_manager =
3128	        std::sync::Arc::new(AuthManager::new(auth_home.path(), GrokComConfig::default()));
3129	    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
3130	    let gateway = GatewaySender::new(tx);
3131	    let mut cfg = AgentConfig::default();
3132	    cfg.plugins.cli_plugin_dirs = vec![plugin_dir.path().to_path_buf()];
3133	    let agent = MvpAgent::new(gateway, &cfg, auth_manager, None, None).expect("valid test config");
3134	    assert!(
3135	        agent.plugin_registry_handle.snapshot().is_none(),
3136	        "snapshot must start empty (boot discovery deferred past initialize)"
3137	    );
3138	    let list_req = acp::ExtRequest::new(
3139	        "x.ai/plugins/list",
3140	        serde_json::value::to_raw_value(&serde_json::json!({ "sessionId": "no-such-session" }))
3141	            .unwrap()
3142	            .into(),
3143	    );
3144	    let resp = crate::extensions::plugins::handle(&agent, &list_req)
3145	        .await
3146	        .expect("plugins/list");
3147	    let listed: serde_json::Value = serde_json::from_str(resp.0.get()).unwrap();
3148	    let names: Vec<&str> = listed
3149	        .get("result")
3150	        .and_then(|r| r.get("plugins"))
3151	        .and_then(|p| p.as_array())
3152	        .expect("plugins array")
3153	        .iter()
3154	        .filter_map(|p| p.get("name").and_then(|n| n.as_str()))
3155	        .collect();
3156	    assert!(
3157	        names.contains(&"regr-lazy-mcp-plugin"),
3158	        "session-less plugins/list must populate the snapshot first, got {names:?}"
3159	    );
3160	    agent.ensure_plugin_registry();
3161	    let snapshot = agent
3162	        .plugin_registry_handle
3163	        .snapshot()
3164	        .expect("snapshot must be populated on first use");
3165	    assert!(
3166	        snapshot.get("regr-lazy-mcp-plugin").is_some(),
3167	        "lazy discovery must surface the plugin so its MCP server merges into the session"
3168	    );
3169	    agent.ensure_plugin_registry();
3170	    assert!(
3171	        agent
3172	            .plugin_registry_handle
3173	            .snapshot()
3174	            .is_some_and(|s| s.get("regr-lazy-mcp-plugin").is_some()),
3175	        "repeat call must keep the populated snapshot"
3176	    );
3177	}
3178	/// Regression: the shared snapshot was built from the boot-time in-memory `[plugins]` config, which
3179	/// `config.toml` edits never refresh. A plugin toggled after the agent started (marketplace install,
3180	/// `grok plugin enable|disable`, a client editing the file) kept its boot-time `enabled` for
3181	/// session-less `x.ai/plugins/list` / `x.ai/skills/list` callers until restart, while per-session
3182	/// registries, which read disk, were right. The shared rebuild must read disk too.
3183	///
3184	/// Exercised through a project `.grok/config.toml` (merged by `resolve_effective_plugins_config` for
3185	/// the given cwd): `grok_home()` is a process-wide `OnceLock`, so the user layer cannot be isolated
3186	/// per test.
3187	#[tokio::test]
3188	async fn shared_plugin_registry_snapshot_reads_plugins_config_from_disk() {
3189	    use crate::agent::config::Config as AgentConfig;
3190	    use xai_grok_login::{AuthManager, GrokComConfig};
3191	    let plugin_dir = tempfile::tempdir().unwrap();
3192	    std::fs::write(
3193	        plugin_dir.path().join("plugin.json"),
3194	        r#"{"name": "regr-disk-disabled"}"#,
3195	    )
3196	    .unwrap();
3197	    let repo = tempfile::tempdir().unwrap();
3198	    git2::Repository::init(repo.path()).unwrap();
3199	    let project_config_dir = repo.path().join(".grok");
3200	    std::fs::create_dir_all(&project_config_dir).unwrap();
3201	    let auth_home = tempfile::tempdir().unwrap();
3202	    let auth_manager =
3203	        std::sync::Arc::new(AuthManager::new(auth_home.path(), GrokComConfig::default()));
3204	    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
3205	    let gateway = GatewaySender::new(tx);
3206	    let mut cfg = AgentConfig::default();
3207	    cfg.plugins.cli_plugin_dirs = vec![plugin_dir.path().to_path_buf()];
3208	    let agent = MvpAgent::new(gateway, &cfg, auth_manager, None, None).expect("valid test config");
3209	    let rebuild = |label: &str| {
3210	        let (trusted, disk_cfg) = MvpAgent::registry_build_inputs(repo.path(), None);
3211	        agent
3212	            .plugin_registry_handle
3213	            .reload(Some(repo.path()), &disk_cfg, trusted, true);
3214	        agent
3215	            .plugin_registry_handle
3216	            .snapshot()
3217	            .unwrap_or_else(|| panic!("{label}: snapshot must be populated"))
3218	            .get("regr-disk-disabled")
3219	            .unwrap_or_else(|| panic!("{label}: plugin must be discovered"))
3220	            .enabled
3221	    };
3222	    assert!(rebuild("baseline"), "nothing disables the plugin yet");
3223	    std::fs::write(
3224	        project_config_dir.join("config.toml"),
3225	        "[plugins]\ndisabled = [\"regr-disk-disabled\"]\n",
3226	    )
3227	    .unwrap();
3228	    assert!(
3229	        !rebuild("after disk edit"),
3230	        "rebuild must take `disabled` from config on disk, not the boot-time config"
3231	    );
3232	}
3233	/// Scaffolding for the session-less `x.ai/plugins/reload` regressions: a hermetic GROK_HOME, the
3234	/// folder-trust feature in its release-build default (`GROK_FOLDER_TRUST` unset), and an agent whose
3235	/// launch dir is `repo` (captured from the process cwd at construction, so callers hold `serial`).
3236	struct ReloadHarness {
3237	    _env: Vec<xai_grok_test_support::EnvGuard>,
3238	    _home: tempfile::TempDir,
3239	    _auth_home: tempfile::TempDir,
3240	    _rx: tokio::sync::mpsc::UnboundedReceiver<<acp::AgentSide as xai_acp_lib::AcpSide>::OutMessage>,
3241	    agent: MvpAgent,
3242	}
3243	impl ReloadHarness {
3244	    fn launched_in(repo: &std::path::Path, cfg: &crate::agent::config::Config) -> Self {
3245	        use xai_grok_login::{AuthManager, GrokComConfig};
3246	        use xai_grok_test_support::EnvGuard;
3247	        let home = tempfile::tempdir().unwrap();
3248	        let env = vec![
3249	            EnvGuard::set("GROK_HOME", home.path()),
3250	            EnvGuard::unset("GROK_FOLDER_TRUST"),
3251	            EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0.0-sim"),
3252	        ];
3253	        let previous_cwd = std::env::current_dir().unwrap();
3254	        std::env::set_current_dir(repo).unwrap();
3255	        let auth_home = tempfile::tempdir().unwrap();
3256	        let auth_manager =
3257	            std::sync::Arc::new(AuthManager::new(auth_home.path(), GrokComConfig::default()));
3258	        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
3259	        let agent = MvpAgent::new(GatewaySender::new(tx), cfg, auth_manager, None, None)
3260	            .expect("valid test config");
3261	        std::env::set_current_dir(previous_cwd).unwrap();
3262	        assert_eq!(
3263	            dunce::canonicalize(&agent.launch_cwd).unwrap(),
3264	            dunce::canonicalize(repo).unwrap(),
3265	            "agent must have captured the repo as its launch dir"
3266	        );
3267	        Self {
3268	            _env: env,
3269	            _home: home,
3270	            _auth_home: auth_home,
3271	            _rx: rx,
3272	            agent,
3273	        }
3274	    }
3275	    async fn reload(&self) {
3276	        let req = acp::ExtRequest::new(
3277	            "x.ai/plugins/reload",
3278	            serde_json::value::to_raw_value(&serde_json::json!({}))
3279	                .unwrap()
3280	                .into(),
3281	        );
3282	        crate::extensions::session_admin::handle(&self.agent, &req)
3283	            .await
3284	            .expect("plugins/reload");
3285	    }
3286	}
3287	fn write_plugin_manifest(dir: &std::path::Path, name: &str) {
3288	    std::fs::create_dir_all(dir).unwrap();
3289	    std::fs::write(dir.join("plugin.json"), format!(r#"{{"name": "{name}"}}"#)).unwrap();
3290	}
3291	/// Kill-switch ordering through the production session-less `x.ai/plugins/reload` path (no resident
3292	/// session, so the rebuild targets the launch dir). The folder-trust gate's cold-key backstop
3293	/// resolves WITHOUT remote settings and records a durable verdict; if it ran before the real-remote
3294	/// resolve, a cold launch dir under an org
3295	/// kill-switch (`folder_trust_enabled = Some(false)`) would be stamped with a kill-switch-blind deny
3296	/// that the store-only reconcile can never lift. Mirrors
3297	/// `kill_switched_cold_cwd_stays_allowed_through_plugins_config_read` for the shared rebuild.
3298	#[tokio::test]
3299	#[serial_test::serial]
3300	async fn plugins_reload_resolves_real_remote_trust_before_reading_disk_config() {
3301	    let repo = tempfile::tempdir().unwrap();
3302	    git2::Repository::init(repo.path()).unwrap();
3303	    let proj_plugin = repo.path().join("proj-plugin");
3304	    write_plugin_manifest(&proj_plugin, "regr-killswitch-proj");
3305	    std::fs::create_dir_all(repo.path().join(".grok")).unwrap();
3306	    std::fs::write(
3307	        repo.path().join(".grok").join("config.toml"),
3308	        format!("[plugins]\npaths = ['{}']\n", proj_plugin.display()),
3309	    )
3310	    .unwrap();
3311	    let cfg = crate::agent::config::Config {
3312	        remote_settings: Some(crate::util::config::RemoteSettings {
3313	            folder_trust_enabled: Some(false),
3314	            ..Default::default()
3315	        }),
3316	        ..Default::default()
3317	    };
3318	    let h = ReloadHarness::launched_in(repo.path(), &cfg);
3319	    h.reload().await;
3320	    assert!(
3321	        crate::agent::folder_trust::project_scope_allowed(&h.agent.launch_cwd),
3322	        "kill-switched cold launch dir must stay allowed after the reload's config read"
3323	    );
3324	    let snapshot = h
3325	        .agent
3326	        .plugin_registry_handle
3327	        .snapshot()
3328	        .expect("reload must populate the snapshot");
3329	    assert!(
3330	        snapshot.get("regr-killswitch-proj").is_some(),
3331	        "kill-switched folder counts trusted, so the project [plugins].paths plugin must be discovered"
3332	    );
3333	}
3334	/// A session-less `x.ai/plugins/reload` must re-resolve the launch dir's folder trust rather than
3335	/// reuse the startup primer's memoized verdict. The primer records a point-in-time answer, and a
3336	/// no-configs launch dir yields a non-durable "trusted" allow; if repo-local plugin configs appear
3337	/// afterwards, a reload that reused that allow would grant them executable trust with no check.
3338	#[tokio::test]
3339	#[serial_test::serial]
3340	async fn plugins_reload_rechecks_launch_dir_trust() {
3341	    let repo = tempfile::tempdir().unwrap();
3342	    git2::Repository::init(repo.path()).unwrap();
3343	    let h = ReloadHarness::launched_in(repo.path(), &crate::agent::config::Config::default());
3344	    h.agent.ensure_plugin_registry();
3345	    assert!(
3346	        crate::agent::folder_trust::project_scope_allowed(&h.agent.launch_cwd),
3347	        "a launch dir without repo-local configs is provisionally allowed"
3348	    );
3349	    write_plugin_manifest(
3350	        &repo
3351	            .path()
3352	            .join(".grok")
3353	            .join("plugins")
3354	            .join("regr-late-proj"),
3355	        "regr-late-proj",
3356	    );
3357	    h.reload().await;
3358	    assert!(
3359	        !crate::agent::folder_trust::project_scope_allowed(&h.agent.launch_cwd),
3360	        "reload must re-resolve trust: the repo now declares code-exec configs and is not store-trusted"
3361	    );
3362	    let snapshot = h
3363	        .agent
3364	        .plugin_registry_handle
3365	        .snapshot()
3366	        .expect("reload must populate the snapshot");
3367	    assert!(
3368	        !snapshot
3369	            .get("regr-late-proj")
3370	            .expect("project plugin must be discovered")
3371	            .trusted,
3372	        "the reload must not grant executable trust to a project plugin the folder-trust gate denies"
3373	    );
3374	    h.agent.ensure_plugin_registry();
3375	    h.agent.ensure_plugin_registry_async().await;
3376	    let plugin_trusted = h
3377	        .agent
3378	        .plugin_registry_handle
3379	        .snapshot()
3380	        .and_then(|s| s.get("regr-late-proj").map(|p| p.trusted));
3381	    assert_eq!(
3382	        plugin_trusted,
3383	        Some(false),
3384	        "ensure_plugin_registry after a reload must keep the reload's verdict"
3385	    );
3386	}
3387	/// The lazy boot build reads disk config through the folder-trust gate. It must not lean on the
3388	/// startup primer: with the gate on, a no-configs launch dir leaves the primer's allow non-durable
3389	/// (nothing recorded), so a later lazy build — reachable from a session-less `x.ai/plugins/list`
3390	/// long after startup, once repo-local configs have appeared and remote settings have moved to the
3391	/// kill-switch — would hit the gate's cold-key backstop, which resolves WITHOUT remote settings and
3392	/// records a kill-switch-blind deny that no reconcile can lift.
3393	#[tokio::test]
3394	#[serial_test::serial]
3395	async fn lazy_registry_build_resolves_real_remote_trust_before_reading_disk_config() {
3396	    let repo = tempfile::tempdir().unwrap();
3397	    git2::Repository::init(repo.path()).unwrap();
3398	    let h = ReloadHarness::launched_in(repo.path(), &crate::agent::config::Config::default());
3399	    assert!(h.agent.prime_launch_dir_trust().1);
3400	    h.agent.cfg.borrow_mut().remote_settings = Some(crate::util::config::RemoteSettings {
3401	        folder_trust_enabled: Some(false),
3402	        ..Default::default()
3403	    });
3404	    write_plugin_manifest(
3405	        &repo
3406	            .path()
3407	            .join(".grok")
3408	            .join("plugins")
3409	            .join("regr-lazy-killswitch"),
3410	        "regr-lazy-killswitch",
3411	    );
3412	    h.agent.ensure_plugin_registry_async().await;
3413	    assert!(
3414	        crate::agent::folder_trust::project_scope_allowed(&h.agent.launch_cwd),
3415	        "kill-switched launch dir must stay allowed after the lazy build's config read"
3416	    );
3417	    let trusted = h
3418	        .agent
3419	        .plugin_registry_handle
3420	        .snapshot()
3421	        .and_then(|s| s.get("regr-lazy-killswitch").map(|p| p.trusted));
3422	    assert_eq!(trusted, Some(true), "kill-switched folder counts trusted");
3423	}
3424	mod list_running_heal_tests;
3425	#[cfg(unix)]
3426	mod process_scope_reclaim;
3427	mod session_rename_tests;
3428	mod session_resume_close_tests;
3429	mod subagent_spawn_context_tests;
3430	/// With no load in flight and no session the wait returns immediately (the caller then surfaces "unknown session id" exactly as before).
3431	#[tokio::test]
3432	async fn wait_for_in_flight_load_returns_immediately_when_idle() {
3433	    let agent = build_minimal_agent_for_tests();
3434	    let sid = acp::SessionId::new("sess-none");
3435	    tokio::time::timeout(
3436	        std::time::Duration::from_millis(200),
3437	        agent.wait_for_in_flight_session_load(&sid),
3438	    )
3439	    .await
3440	    .expect("wait must not block when no load is in flight");
3441	}
3442	/// A waiter racing an in-flight `session/load` blocks until the load finishes and then observes the registered session.
3443	/// This is the agent-side guarantee that closes the post-leader-crash "unknown session id" race.
3444	/// The reconnect replay's `session/load` and the client's next `session/prompt` can arrive back-to-back.
3445	#[tokio::test]
3446	async fn wait_for_in_flight_load_blocks_until_load_completes() {
3447	    let local = tokio::task::LocalSet::new();
3448	    local
3449	        .run_until(async {
3450	            let agent = std::rc::Rc::new(build_minimal_agent_for_tests());
3451	            let sid = acp::SessionId::new("sess-loading");
3452	            let guard = agent.begin_session_load(&sid);
3453	            let waiter_agent = agent.clone();
3454	            let waiter_sid = sid.clone();
3455	            let waiter = tokio::task::spawn_local(async move {
3456	                waiter_agent
3457	                    .wait_for_in_flight_session_load(&waiter_sid)
3458	                    .await;
3459	                waiter_agent.is_resident(&waiter_sid)
3460	            });
3461	            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
3462	            assert!(!waiter.is_finished(), "waiter must block while loading");
3463	            let handle = make_test_handle("test-model", false, None);
3464	            agent.insert_resident(&sid, handle);
3465	            drop(guard);
3466	            let found_session = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
3467	                .await
3468	                .expect("waiter must wake when the load guard drops")
3469	                .expect("waiter task must not panic");
3470	            assert!(
3471	                found_session,
3472	                "after the wait, the session must be visible to the racing request"
3473	            );
3474	        })
3475	        .await;
3476	}
3477	/// A failed load (guard dropped WITHOUT registering the session) also wakes waiters.
3478	/// They re-check, find nothing, and the caller surfaces the regular "unknown session id" error rather than hanging.
3479	#[tokio::test]
3480	async fn wait_for_in_flight_load_wakes_on_failed_load() {
3481	    let local = tokio::task::LocalSet::new();
3482	    local
3483	        .run_until(async {
3484	            let agent = std::rc::Rc::new(build_minimal_agent_for_tests());
3485	            let sid = acp::SessionId::new("sess-load-fails");
3486	            let guard = agent.begin_session_load(&sid);
3487	            let waiter_agent = agent.clone();
3488	            let waiter_sid = sid.clone();
3489	            let waiter = tokio::task::spawn_local(async move {
3490	                waiter_agent
3491	                    .wait_for_in_flight_session_load(&waiter_sid)
3492	                    .await;
3493	                waiter_agent.is_resident(&waiter_sid)
3494	            });
3495	            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
3496	            drop(guard);
3497	            let found_session = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
3498	                .await
3499	                .expect("waiter must wake when the failed load's guard drops")
3500	                .expect("waiter task must not panic");
3501	            assert!(!found_session, "failed load leaves no session behind");
3502	        })
3503	        .await;
3504	}
3505	/// Two concurrent loads of the same session: the first guard's drop must not remove the second load's marker.
3506	/// Waiters keep waiting on the newer in-flight load.
3507	#[tokio::test]
3508	async fn concurrent_load_guards_do_not_clobber_each_other() {
3509	    let agent = build_minimal_agent_for_tests();
3510	    let sid = acp::SessionId::new("sess-concurrent");
3511	    let guard_one = agent.begin_session_load(&sid);
3512	    let guard_two = agent.begin_session_load(&sid);
3513	    drop(guard_one);
3514	    assert!(
3515	        agent.session_registry.is_attaching(&sid),
3516	        "second load's marker must survive the first guard's drop"
3517	    );
3518	    drop(guard_two);
3519	    assert!(
3520	        agent.session_registry.attaching_count() == 0,
3521	        "all markers removed once every load finished"
3522	    );
3523	}
3524	/// `resident_activity` returns `NeedsInput` whenever the session's pending-interaction map is non-empty.
3525	/// That wins even over a running turn (a session blocked on a permission mid-turn "needs input").
3526	/// Clearing the map falls back to Working / Idle.
3527	#[tokio::test]
3528	async fn resident_activity_reports_needs_input_when_pending() {
3529	    use crate::agent::roster::RosterActivity;
3530	    let agent = build_minimal_agent_for_tests();
3531	    let sid = acp::SessionId::new("sess-pending");
3532	    let handle = make_test_handle("grok-3", false, None);
3533	    let pending = handle.pending_interactions.clone();
3534	    let prompt_id = handle.current_prompt_id.clone();
3535	    agent.insert_resident(&sid, handle);
3536	    assert_eq!(agent.resident_activity(&sid), RosterActivity::Idle);
3537	    *prompt_id.lock().unwrap() = Some("turn-1".to_string());
3538	    assert_eq!(agent.resident_activity(&sid), RosterActivity::Working);
3539	    pending.lock().unwrap().insert(
3540	        "call-1".to_string(),
3541	        crate::session::pending_interaction::PendingKind::Permission,
3542	    );
3543	    assert_eq!(agent.resident_activity(&sid), RosterActivity::NeedsInput);
3544	    let entry = agent.resident_roster_entry(&sid).expect("resident entry");
3545	    assert_eq!(entry.activity, RosterActivity::NeedsInput);
3546	    pending.lock().unwrap().clear();
3547	    assert_eq!(agent.resident_activity(&sid), RosterActivity::Working);
3548	}
3549	/// Drain the agent gateway, returning the first `x.ai/sessions/changed` payload that carries an upserted entry.
3550	/// Unrelated notifications parse into an empty `RosterChanged` and are ignored.
3551	fn drain_roster_changed(
3552	    rx: &mut tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
3553	) -> Option<crate::agent::roster::RosterChanged> {
3554	    let mut found = None;
3555	    while let Ok(msg) = rx.try_recv() {
3556	        if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg {
3557	            if found.is_none()
3558	                && let Ok(changed) = serde_json::from_str::<crate::agent::roster::RosterChanged>(
3559	                    args.request.params.get(),
3560	                )
3561	                && !changed.upserted.is_empty()
3562	            {
3563	                found = Some(changed);
3564	            }
3565	            let _ = args.response_tx.send(Ok(()));
3566	        }
3567	    }
3568	    found
3569	}
3570	/// A turn-boundary activity delta (`push_roster_activity_delta`) broadcasts an `x.ai/sessions/changed` upsert carrying the *overridden* activity.
3571	/// Every attached dashboard then reflects Working/Idle immediately instead of waiting out the roster poll's up-to-1s lag (turn-start/turn-end).
3572	/// The override matters because at turn-start the actor has not yet published `current_prompt_id`. A natural `resident_activity` read would emit `Idle` for a session that is in fact starting a turn.
3573	#[tokio::test]
3574	async fn headless_residents_are_excluded_from_snapshots_and_deltas() {
3575	    use crate::agent::config::Config as AgentConfig;
3576	    use crate::agent::roster::RosterActivity;
3577	    use xai_grok_login::{AuthManager, GrokComConfig};
3578	    let temp_dir = tempfile::tempdir().unwrap();
3579	    let auth_manager =
3580	        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
3581	    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
3582	    let gateway = GatewaySender::new(tx);
3583	    let agent = MvpAgent::new(gateway, &AgentConfig::default(), auth_manager, None, None)
3584	        .expect("valid test config");
3585	    let sid = acp::SessionId::new("sess-headless");
3586	    agent.insert_resident(&sid, make_test_handle("grok-3", false, None));
3587	    agent.session_registry.mark_headless(&sid);
3588	    assert!(agent.resident_roster_entry(&sid).is_none());
3589	    assert!(agent.resident_roster_entries().is_empty());
3590	    agent.push_roster_delta_upserted(&sid);
3591	    agent.push_roster_activity_delta(&sid, RosterActivity::Working);
3592	    assert!(drain_roster_changed(&mut rx).is_none());
3593	}
3594	#[tokio::test]
3595	async fn push_roster_activity_delta_broadcasts_overridden_activity() {
3596	    use crate::agent::config::Config as AgentConfig;
3597	    use crate::agent::roster::RosterActivity;
3598	    use xai_grok_login::{AuthManager, GrokComConfig};
3599	    let temp_dir = tempfile::tempdir().unwrap();
3600	    let auth_manager =
3601	        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
3602	    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
3603	    let gateway = GatewaySender::new(tx);
3604	    let cfg = AgentConfig::default();
3605	    let agent = MvpAgent::new(gateway, &cfg, auth_manager, None, None).expect("valid test config");
3606	    let sid = acp::SessionId::new("sess-activity");
3607	    agent.insert_resident(&sid, make_test_handle("grok-3", false, None));
3608	    agent.push_roster_activity_delta(&sid, RosterActivity::Working);
3609	    let changed = drain_roster_changed(&mut rx).expect("turn-start delta emitted");
3610	    assert_eq!(changed.upserted.len(), 1);
3611	    let Some(first) = changed.upserted.first() else {
3612	        panic!("expected one upserted roster row: {changed:?}");
3613	    };
3614	    assert_eq!(first.session_id, sid.0.to_string());
3615	    assert!(first.resident);
3616	    assert_eq!(
3617	        first.activity,
3618	        RosterActivity::Working,
3619	        "forced activity must override the Idle that resident_activity would read"
3620	    );
3621	    assert!(changed.removed.is_empty());
3622	    agent.push_roster_activity_delta(&sid, RosterActivity::Idle);
3623	    let changed = drain_roster_changed(&mut rx).expect("turn-end delta emitted");
3624	    let Some(idle) = changed.upserted.first() else {
3625	        panic!("expected one upserted roster row: {changed:?}");
3626	    };
3627	    assert_eq!(idle.activity, RosterActivity::Idle);
3628	}
3629	#[expect(
3630	    dead_code,
3631	    reason = "unused in production; remove expect when wired or delete the item"
3632	)]
3633	fn parse_ext_body(resp: &acp::ExtResponse) -> serde_json::Value {
3634	    let outer: serde_json::Value =
3635	        serde_json::from_str(resp.0.get()).expect("ExtResponse must be valid JSON");
3636	    outer
3637	        .get("result")
3638	        .cloned()
3639	        .unwrap_or_else(|| panic!("ExtResponse has no 'result' key; full JSON: {outer}"))
3640	}
3641	/// Replicate the lookup logic of code_nav_eligibility_for_request so we can test it with a plain sessions HashMap.
3642	fn check_nav_eligibility_from_sessions(
3643	    sessions: &HashMap<acp::SessionId, crate::session::SessionHandle>,
3644	    session_id: Option<&acp::SessionId>,
3645	) -> Result<(), CodeNavEligibility> {
3646	    let session_id = match session_id {
3647	        Some(sid) => sid,
3648	        None => return Err(CodeNavEligibility::SessionRequired),
3649	    };
3650	    let Some(handle) = sessions.get(session_id) else {
3651	        return Err(CodeNavEligibility::SessionRequired);
3652	    };
3653	    let ct = crate::http::client_type_from_origin(handle.origin_client.as_ref());
3654	    if !matches!(ct, ClientType::GrokWeb) {
3655	        return Err(CodeNavEligibility::ClientNotWeb);
3656	    }
3657	    if !handle.code_nav_enabled {
3658	        return Err(CodeNavEligibility::CapabilityNotAdvertised);
3659	    }
3660	    Ok(())
3661	}
3662	/// This is the "happy path" that allows lazy index startup on the first code-nav request.
3663	#[tokio::test]
3664	async fn test_web_session_with_capability_is_eligible() {
3665	    let sid = acp::SessionId::new("sess-web");
3666	    let mut handle = make_test_handle("model", false, Some("grok-web"));
3667	    handle.code_nav_enabled = true;
3668	    let sessions = [(sid.clone(), handle)].into();
3669	    assert!(
3670	        check_nav_eligibility_from_sessions(&sessions, Some(&sid)).is_ok(),
3671	        "web session with code-nav capability must be eligible"
3672	    );
3673	}
3674	/// TUI session is rejected at gate 1 (client type) regardless of capability.
3675	#[tokio::test]
3676	async fn test_tui_session_is_rejected() {
3677	    let sid = acp::SessionId::new("sess-tui");
3678	    let mut handle = make_test_handle("model", false, Some("grok-tui"));
3679	    handle.code_nav_enabled = true;
3680	    let sessions = [(sid.clone(), handle)].into();
3681	    assert_eq!(
3682	        check_nav_eligibility_from_sessions(&sessions, Some(&sid)),
3683	        Err(CodeNavEligibility::ClientNotWeb),
3684	        "TUI client must be rejected at gate 1 (client type)"
3685	    );
3686	}
3687	/// Web session without capability is rejected at gate 2.
3688	#[tokio::test]
3689	async fn test_web_session_without_capability_is_rejected() {
3690	    let sid = acp::SessionId::new("sess-web-no-cap");
3691	    let mut handle = make_test_handle("model", false, Some("grok-web"));
3692	    handle.code_nav_enabled = false;
3693	    let sessions = [(sid.clone(), handle)].into();
3694	    assert_eq!(
3695	        check_nav_eligibility_from_sessions(&sessions, Some(&sid)),
3696	        Err(CodeNavEligibility::CapabilityNotAdvertised),
3697	        "web client without capability must be rejected at gate 2"
3698	    );
3699	}
3700	/// Leader-mode isolation: two sessions with different code-nav state return independent results.
3701	#[tokio::test]
3702	async fn test_leader_mode_two_sessions_stay_isolated() {
3703	    let web_sid = acp::SessionId::new("web");
3704	    let tui_sid = acp::SessionId::new("tui");
3705	    let mut web_handle = make_test_handle("model", false, Some("grok-web"));
3706	    web_handle.code_nav_enabled = true;
3707	    let mut tui_handle = make_test_handle("model", false, Some("grok-tui"));
3708	    tui_handle.code_nav_enabled = false;
3709	    let sessions = [(web_sid.clone(), web_handle), (tui_sid.clone(), tui_handle)].into();
3710	    assert!(
3711	        check_nav_eligibility_from_sessions(&sessions, Some(&web_sid)).is_ok(),
3712	        "web session must be eligible"
3713	    );
3714	    assert_eq!(
3715	        check_nav_eligibility_from_sessions(&sessions, Some(&tui_sid)),
3716	        Err(CodeNavEligibility::ClientNotWeb),
3717	        "tui session must remain ineligible even when web session is eligible"
3718	    );
3719	}
3720	/// This is the stale/evicted session path: a session ID that no longer exists in the sessions map gets SessionRequired.
3721	/// It must not inherit the last-initialized client's eligibility.
3722	#[tokio::test]
3723	async fn test_unknown_session_id_returns_session_required() {
3724	    let known_sid = acp::SessionId::new("known");
3725	    let mut known_handle = make_test_handle("model", false, Some("grok-web"));
3726	    known_handle.code_nav_enabled = true;
3727	    let sessions = [(known_sid.clone(), known_handle)].into();
3728	    let stale_sid = acp::SessionId::new("stale-or-evicted");
3729	    assert_eq!(
3730	        check_nav_eligibility_from_sessions(&sessions, Some(&stale_sid)),
3731	        Err(CodeNavEligibility::SessionRequired),
3732	        "stale/evicted sessionId must not fall back to global state"
3733	    );
3734	    assert!(check_nav_eligibility_from_sessions(&sessions, Some(&known_sid)).is_ok());
3735	}
3736	mod parse_json_object_env_tests {
3737	    use crate::util::parse_json_object_env;
3738	    unsafe fn set(k: &str, v: &str) {
3739	        unsafe { std::env::set_var(k, v) };
3740	    }
3741	    unsafe fn unset(k: &str) {
3742	        unsafe { std::env::remove_var(k) };
3743	    }
3744	    #[test]
3745	    #[serial_test::serial]
3746	    fn valid_json_object_returns_some() {
3747	        unsafe { set("TEST_JSON_OBJ", r#"{"team":"platform","org":"acme"}"#) };
3748	        let result = parse_json_object_env("TEST_JSON_OBJ");
3749	        unsafe { unset("TEST_JSON_OBJ") };
3750	        let val = result.expect("should parse valid JSON object");
3751	        assert_eq!(val.get("team").and_then(|v| v.as_str()), Some("platform"));
3752	        assert_eq!(val.get("org").and_then(|v| v.as_str()), Some("acme"));
3753	    }
3754	    #[test]
3755	    #[serial_test::serial]
3756	    fn non_object_json_returns_none() {
3757	        unsafe { set("TEST_JSON_ARR", r#"["not","an","object"]"#) };
3758	        let result = parse_json_object_env("TEST_JSON_ARR");
3759	        unsafe { unset("TEST_JSON_ARR") };
3760	        assert!(result.is_none());
3761	    }
3762	    #[test]
3763	    #[serial_test::serial]
3764	    fn invalid_json_returns_none() {
3765	        unsafe { set("TEST_JSON_BAD", "not json at all") };
3766	        let result = parse_json_object_env("TEST_JSON_BAD");
3767	        unsafe { unset("TEST_JSON_BAD") };
3768	        assert!(result.is_none());
3769	    }
3770	    #[test]
3771	    #[serial_test::serial]
3772	    fn unset_var_returns_none() {
3773	        unsafe { unset("TEST_JSON_UNSET") };
3774	        assert!(parse_json_object_env("TEST_JSON_UNSET").is_none());
3775	    }
3776	}
3777	#[test]
3778	fn find_model_by_id_prefers_key_then_falls_back_to_slug() {
3779	    let entry = |model: &str| ModelEntry {
3780	        bundled_catalog_entry: false,
3781	        official_catalog_entry: false,
3782	        info: config::ModelInfo {
3783	            model: model.to_string(),
3784	            context_window: std::num::NonZeroU64::new(200_000).unwrap(),
3785	            ..Default::default()
3786	        },
3787	        mtls_cert_dir: None,
3788	        api_key: None,
3789	        env_key: None,
3790	        auth_provider: None,
3791	        api_base_url: None,
3792	    };
3793	    let mut models = indexmap::IndexMap::new();
3794	    models.insert("a".to_string(), entry("target"));
3795	    models.insert("target".to_string(), entry("other"));
3796	    assert_eq!(
3797	        config::find_model_by_id(&models, "target").unwrap().model,
3798	        "other",
3799	        "key match should win over slug scan"
3800	    );
3801	    assert_eq!(
3802	        config::find_model_by_id(&models, "a").unwrap().model,
3803	        "target",
3804	        "exact key match for 'a'"
3805	    );
3806	}
3807	fn write_updates(dir: &std::path::Path, lines: &[&str]) -> PathBuf {
3808	    let path = dir.join("updates.jsonl");
3809	    std::fs::write(&path, lines.join("\n")).unwrap();
3810	    path
3811	}
3812	fn bg_line(task_id: &str) -> String {
3813	    format!(
3814	        r#"{{"timestamp":1,"method":"_x.ai/session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"task_backgrounded","task_id":"{task_id}","command":"sleep 99","cwd":"/tmp"}}}}}}"#
3815	    )
3816	}
3817	fn completed_line(task_id: &str) -> String {
3818	    format!(
3819	        r#"{{"timestamp":2,"method":"_x.ai/session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"task_completed","task_snapshot":{{"task_id":"{task_id}","completed":true}}}}}}}}"#
3820	    )
3821	}
3822	fn orphaned_ids(tasks: &[OrphanedTask]) -> std::collections::HashSet<&str> {
3823	    tasks.iter().map(|t| t.task_id.as_str()).collect()
3824	}
3825	#[test]
3826	fn orphaned_tasks_returns_empty_for_no_file() {
3827	    let result = MvpAgent::find_orphaned_background_tasks(&None);
3828	    assert!(result.is_empty());
3829	}
3830	#[test]
3831	fn orphaned_tasks_returns_empty_for_missing_file() {
3832	    let path = PathBuf::from("/nonexistent/updates.jsonl");
3833	    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
3834	    assert!(result.is_empty());
3835	}
3836	#[test]
3837	fn orphaned_tasks_returns_empty_when_all_completed() {
3838	    let tmp = tempfile::tempdir().unwrap();
3839	    let bg = bg_line("t1");
3840	    let done = completed_line("t1");
3841	    let path = write_updates(tmp.path(), &[&bg, &done]);
3842	    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
3843	    assert!(result.is_empty());
3844	}
3845	#[test]
3846	fn orphaned_tasks_returns_uncompleted() {
3847	    let tmp = tempfile::tempdir().unwrap();
3848	    let bg1 = bg_line("t1");
3849	    let bg2 = bg_line("t2");
3850	    let done1 = completed_line("t1");
3851	    let path = write_updates(tmp.path(), &[&bg1, &bg2, &done1]);
3852	    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
3853	    let ids = orphaned_ids(&result);
3854	    assert_eq!(ids.len(), 1);
3855	    assert!(ids.contains("t2"));
3856	}
3857	#[test]
3858	fn orphaned_tasks_returns_multiple_uncompleted() {
3859	    let tmp = tempfile::tempdir().unwrap();
3860	    let bg1 = bg_line("t1");
3861	    let bg2 = bg_line("t2");
3862	    let bg3 = bg_line("t3");
3863	    let done2 = completed_line("t2");
3864	    let path = write_updates(tmp.path(), &[&bg1, &bg2, &bg3, &done2]);
3865	    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
3866	    let ids = orphaned_ids(&result);
3867	    assert_eq!(ids.len(), 2);
3868	    assert!(ids.contains("t1"));
3869	    assert!(ids.contains("t3"));
3870	}
3871	#[test]
3872	fn orphaned_tasks_captures_command_and_cwd() {
3873	    let tmp = tempfile::tempdir().unwrap();
3874	    let bg = bg_line("t1");
3875	    let path = write_updates(tmp.path(), &[&bg]);
3876	    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
3877	    assert_eq!(result.len(), 1);
3878	    let Some(task) = result.first() else {
3879	        panic!("expected one orphaned task");
3880	    };
3881	    assert_eq!(task.command, "sleep 99");
3882	    assert_eq!(task.cwd, "/tmp");
3883	}
3884	#[test]
3885	fn orphaned_tasks_skips_malformed_lines() {
3886	    let tmp = tempfile::tempdir().unwrap();
3887	    let bg = bg_line("t1");
3888	    let path = write_updates(tmp.path(), &["not json", &bg, "{}"]);
3889	    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
3890	    assert_eq!(result.len(), 1);
3891	}
3892	#[test]
3893	fn orphaned_tasks_ignores_unrelated_updates() {
3894	    let tmp = tempfile::tempdir().unwrap();
3895	    let bg = bg_line("t1");
3896	    let unrelated = r#"{"timestamp":1,"method":"_x.ai/session/update","params":{"sessionId":"s","update":{"sessionUpdate":"auto_compact_started","percentage":80}}}"#;
3897	    let path = write_updates(tmp.path(), &[&bg, unrelated]);
3898	    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
3899	    assert_eq!(result.len(), 1);
3900	}
3901	#[test]
3902	fn orphaned_tasks_filters_rewind_dead_branches() {
3903	    let tmp = tempfile::tempdir().unwrap();
3904	    let user_msg = r#"{"timestamp":0,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello"}}}}"#;
3905	    let bg_before_rewind = bg_line("t-dead");
3906	    let rewind = r#"{"timestamp":3,"method":"_x.ai/session/update","params":{"sessionId":"s","update":{"sessionUpdate":"rewind_marker","target_prompt_index":0,"created_at":"2025-01-01T00:00:00Z"}}}"#;
3907	    let user_msg2 = r#"{"timestamp":4,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"retry"}}}}"#;
3908	    let bg_after_rewind = bg_line("t-alive");
3909	    let path = write_updates(
3910	        tmp.path(),
3911	        &[
3912	            user_msg,
3913	            &bg_before_rewind,
3914	            rewind,
3915	            user_msg2,
3916	            &bg_after_rewind,
3917	        ],
3918	    );
3919	    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
3920	    let ids = orphaned_ids(&result);
3921	    assert!(
3922	        ids.contains("t-alive"),
3923	        "task after rewind should be present"
3924	    );
3925	    assert!(
3926	        !ids.contains("t-dead"),
3927	        "task in dead branch should be filtered"
3928	    );
3929	}
3930	#[test]
3931	fn allow_access_from_remote_settings() {
3932	    let json = serde_json::json!({ "allow_access": true });
3933	    let rs: crate::util::config::RemoteSettings = serde_json::from_value(json).unwrap();
3934	    assert_eq!(rs.allow_access, Some(true));
3935	    let json = serde_json::json!({ "allow_access": false });
3936	    let rs: crate::util::config::RemoteSettings = serde_json::from_value(json).unwrap();
3937	    assert_eq!(rs.allow_access, Some(false));
3938	    let json = serde_json::json!({});
3939	    let rs: crate::util::config::RemoteSettings = serde_json::from_value(json).unwrap();
3940	    assert_eq!(rs.allow_access, None);
3941	}
3942	#[test]
3943	fn on_demand_enabled_from_remote_settings() {
3944	    let json = serde_json::json!({ "on_demand_enabled": false });
3945	    let rs: crate::util::config::RemoteSettings = serde_json::from_value(json).unwrap();
3946	    assert_eq!(rs.on_demand_enabled, Some(false));
3947	    let json = serde_json::json!({});
3948	    let rs: crate::util::config::RemoteSettings = serde_json::from_value(json).unwrap();
3949	    assert_eq!(rs.on_demand_enabled, None);
3950	}
3951	/// Regression for a 401 sequence seen in production. After a long idle window, the auth manager may have no live token by the time `session/new` runs.
3952	/// For session-based auth methods we MUST still report `SessionToken` so chat_state credentials retain the session-token shape. `try_refresh_session_token` then runs on the next prompt instead of early-returning.
3953	#[tokio::test(flavor = "current_thread")]
3954	async fn auth_type_session_based_no_current_returns_session_token() {
3955	    for method_id in [
3956	        crate::agent::auth_method::CACHED_TOKEN_AUTH_METHOD_ID,
3957	        crate::agent::auth_method::GROK_COM_METHOD_ID,
3958	        crate::agent::auth_method::OIDC_METHOD_ID,
3959	    ] {
3960	        let agent = build_minimal_agent_for_tests();
3961	        agent.set_auth_method(acp::AuthMethodId::new(method_id));
3962	        assert!(
3963	            agent.auth_manager.current().is_none(),
3964	            "{method_id}: precondition: AuthManager has no current token",
3965	        );
3966	        assert_eq!(
3967	            agent.auth_type(),
3968	            xai_chat_state::AuthType::SessionToken,
3969	            "{method_id}: session-based auth must report SessionToken even \
3970	                 without a live token -- otherwise chat_state gets locked into \
3971	                 auth_type = ApiKey and try_refresh_session_token will skip \
3972	                 every subsequent refresh attempt.",
3973	        );
3974	    }
3975	}
3976	/// BYOK guard. Users with `xai.api_key` must continue to report `ApiKey` regardless of live-token state.
3977	/// BYOK sessions have nothing to refresh.
3978	/// Reporting `SessionToken` would route through cli-chat-proxy paths (image_gen / video_gen base_url) that don't apply to BYOK keys.
3979	#[tokio::test(flavor = "current_thread")]
3980	async fn auth_type_xai_api_key_no_current_returns_api_key() {
3981	    let agent = build_minimal_agent_for_tests();
3982	    agent.set_auth_method(acp::AuthMethodId::new(
3983	        crate::agent::auth_method::XAI_API_KEY_METHOD_ID,
3984	    ));
3985	    assert!(agent.auth_manager.current().is_none());
3986	    assert_eq!(
3987	        agent.auth_type(),
3988	        xai_chat_state::AuthType::ApiKey,
3989	        "xai.api_key auth must report ApiKey -- BYOK has no session-token \
3990	             behavior to fall back to."
3991	    );
3992	}
3993	/// Positive baseline: when both signals agree (session-based method AND a live in-memory token), `SessionToken` is returned.
3994	/// This is the common case during a healthy session.
3995	#[tokio::test(flavor = "current_thread")]
3996	async fn auth_type_session_based_with_current_returns_session_token() {
3997	    use xai_grok_login::GrokAuth;
3998	    let agent = build_minimal_agent_for_tests();
3999	    agent.set_auth_method(acp::AuthMethodId::new(
4000	        crate::agent::auth_method::OIDC_METHOD_ID,
4001	    ));
4002	    agent.auth_manager.hot_swap(GrokAuth::test_default());
4003	    assert!(agent.auth_manager.current().is_some());
4004	    assert_eq!(agent.auth_type(), xai_chat_state::AuthType::SessionToken,);
4005	}
4006	/// Defensive case: no `auth_method_id` selected yet (pre-`authenticate` state) and no live credential. We default to `ApiKey`. Callers key off this value (e.g. `resolve_chat_state_auth_type` for chat routing).
4007	/// Any other default would route session-token-shaped traffic through cli-chat-proxy before a method has been chosen.
4008	#[tokio::test(flavor = "current_thread")]
4009	async fn auth_type_no_method_id_no_current_returns_api_key() {
4010	    let agent = build_minimal_agent_for_tests();
4011	    assert!(agent.auth_method_id.load().is_none());
4012	    assert!(agent.auth_manager.current().is_none());
4013	    assert_eq!(agent.auth_type(), xai_chat_state::AuthType::ApiKey,);
4014	}
4015	/// Live credential present but `auth_method_id` is still `None`. The in-memory bearer takes precedence: this is the order observed during `initialize()` silent refresh.
4016	/// A token is hot-swapped in before `authenticate()` writes the method id. Reporting `SessionToken` here matches pre-fix behavior and keeps logging stable.
4017	#[tokio::test(flavor = "current_thread")]
4018	async fn auth_type_no_method_id_with_current_returns_session_token() {
4019	    use xai_grok_login::GrokAuth;
4020	    let agent = build_minimal_agent_for_tests();
4021	    agent.auth_manager.hot_swap(GrokAuth::test_default());
4022	    assert!(agent.auth_method_id.load().is_none());
4023	    assert!(agent.auth_manager.current().is_some());
4024	    assert_eq!(agent.auth_type(), xai_chat_state::AuthType::SessionToken,);
4025	}
4026	/// Minimal agent whose `grok_com_config` engages the api-key kill switch (`disable_api_key_auth = true`), mirroring a forced-IdP deployment.
4027	fn build_agent_with_api_key_auth_disabled() -> MvpAgent {
4028	    use crate::agent::config::Config as AgentConfig;
4029	    use xai_grok_login::{AuthManager, GrokComConfig};
4030	    let temp_dir = tempfile::tempdir().unwrap();
4031	    let auth_manager =
4032	        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
4033	    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
4034	    let gateway = GatewaySender::new(tx);
4035	    let mut cfg = AgentConfig::default();
4036	    cfg.grok_com_config.disable_api_key_auth = Some(true);
4037	    MvpAgent::new(gateway, &cfg, auth_manager, None, None).expect("valid test config")
4038	}
4039	/// Deployment-key / managed-config user: `XAI_API_KEY` resolves and the kill switch is off.
4040	/// A dead `cached_token` MUST then fall through to `xai.api_key` (no browser).
4041	/// This is the exact regression the fallthrough fixes.
4042	#[tokio::test(flavor = "current_thread")]
4043	#[serial_test::serial]
4044	async fn cached_token_fallthrough_prefers_api_key_for_deployment_key() {
4045	    use crate::agent::auth_method::{XAI_API_KEY_ENV_VAR, XAI_API_KEY_METHOD_ID};
4046	    use xai_grok_test_support::EnvGuard;
4047	    let _lockdown = EnvGuard::unset("GROK_DISABLE_API_KEY_AUTH");
4048	    let _key = EnvGuard::set(XAI_API_KEY_ENV_VAR, "test-deployment-key");
4049	    let agent = build_minimal_agent_for_tests();
4050	    assert_eq!(
4051	        agent
4052	            .cached_token_fallthrough_method_id()
4053	            .as_ref()
4054	            .map(|id| id.0.as_ref()),
4055	        Some(XAI_API_KEY_METHOD_ID),
4056	        "deployment-key user (XAI_API_KEY set, no kill switch) must fall \
4057	         through to xai.api_key on a dead cached_token -- not interactive login",
4058	    );
4059	}
4060	/// Forced-IdP deployment: even with `XAI_API_KEY` present, the admin kill switch keeps the fallthrough on interactive `grok.com`.
4061	/// Api-key auth is neither advertised nor an eligible fallthrough.
4062	#[tokio::test(flavor = "current_thread")]
4063	#[serial_test::serial]
4064	async fn cached_token_fallthrough_respects_kill_switch() {
4065	    use crate::agent::auth_method::{GROK_COM_METHOD_ID, XAI_API_KEY_ENV_VAR};
4066	    use xai_grok_test_support::EnvGuard;
4067	    let _lockdown = EnvGuard::unset("GROK_DISABLE_API_KEY_AUTH");
4068	    let _key = EnvGuard::set(XAI_API_KEY_ENV_VAR, "test-deployment-key");
4069	    let agent = build_agent_with_api_key_auth_disabled();
4070	    assert_eq!(
4071	        agent
4072	            .cached_token_fallthrough_method_id()
4073	            .as_ref()
4074	            .map(|id| id.0.as_ref()),
4075	        Some(GROK_COM_METHOD_ID),
4076	        "disable_api_key_auth must keep the cached_token fallthrough on \
4077	         interactive grok.com so XAI_API_KEY can't bypass forced IdP login",
4078	    );
4079	}
4080	/// No advertiseable credentials at all (no env key, no kill switch): the user genuinely needs to log in.
4081	/// The fallthrough is interactive `grok.com`.
4082	#[tokio::test(flavor = "current_thread")]
4083	#[serial_test::serial]
4084	async fn cached_token_fallthrough_falls_to_grok_com_without_credentials() {
4085	    use crate::agent::auth_method::{
4086	        GROK_COM_METHOD_ID, LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR,
4087	    };
4088	    use xai_grok_test_support::EnvGuard;
4089	    let _lockdown = EnvGuard::unset("GROK_DISABLE_API_KEY_AUTH");
4090	    let _new = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
4091	    let _legacy = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);
4092	    let agent = build_minimal_agent_for_tests();
4093	    assert_eq!(
4094	        agent
4095	            .cached_token_fallthrough_method_id()
4096	            .as_ref()
4097	            .map(|id| id.0.as_ref()),
4098	        Some(GROK_COM_METHOD_ID),
4099	        "no API-key creds and no kill switch -> interactive grok.com login",
4100	    );
4101	}
4102	/// Verifies the 4-state matrix of `(disable_zdr_incompatible_tools, zdr_video_output_s3)`: | ZDR flag | S3 config | Result | |----------|-----------|---------------------------------------------| | false | None | Enabled, no S3 (normal non-ZDR mode) | | true | None | Disabled (ZDR with no escape hatch) | | false | Some | Enabled, S3 **not** threaded (non-ZDR) | | true | Some | Enabled, S3 threaded (ZDR with upload path) |
4103	#[tokio::test(flavor = "current_thread")]
4104	async fn prepare_video_gen_config_disabled_when_zdr_flag_set() {
4105	    use xai_grok_tools::implementations::grok_build::video_gen::{
4106	        S3AccessCredentials, VideoGenConfig, ZdrVideoOutputS3Config,
4107	    };
4108	    fn zdr_s3() -> ZdrVideoOutputS3Config {
4109	        ZdrVideoOutputS3Config {
4110	            bucket: "team-videos".into(),
4111	            endpoint: "https://s3.example.com".into(),
4112	            region: "us-east-1".into(),
4113	            key_prefix: "grok-videos/".into(),
4114	            expires_secs: 900,
4115	            read_write: S3AccessCredentials {
4116	                access_key_id: "AKIA...".into(),
4117	                secret_access_key: "secret".into(),
4118	            },
4119	            read_only: None,
4120	        }
4121	    }
4122	    let agent = build_minimal_agent_for_tests();
4123	    agent.sampling_config.borrow_mut().api_key = Some("test-key".to_string());
4124	    assert!(matches!(
4125	        agent.prepare_video_gen_config(),
4126	        VideoGenConfig::Enabled { .. }
4127	    ));
4128	    agent.cfg.borrow_mut().disable_zdr_incompatible_tools = true;
4129	    assert!(matches!(
4130	        agent.prepare_video_gen_config(),
4131	        VideoGenConfig::Enabled {
4132	            zdr_restricted: true,
4133	            ..
4134	        }
4135	    ));
4136	    agent.cfg.borrow_mut().zdr_video_output_s3 = Some(zdr_s3());
4137	    agent.cfg.borrow_mut().disable_zdr_incompatible_tools = false;
4138	    let VideoGenConfig::Enabled {
4139	        zdr_video_output_s3: s3_when_non_zdr,
4140	        ..
4141	    } = agent.prepare_video_gen_config()
4142	    else {
4143	        panic!("expected Enabled");
4144	    };
4145	    assert!(
4146	        s3_when_non_zdr.is_none(),
4147	        "S3 config must not be threaded when ZDR flag is off"
4148	    );
4149	    agent.cfg.borrow_mut().disable_zdr_incompatible_tools = true;
4150	    let VideoGenConfig::Enabled {
4151	        zdr_video_output_s3,
4152	        zdr_restricted,
4153	        ..
4154	    } = agent.prepare_video_gen_config()
4155	    else {
4156	        panic!("expected Enabled");
4157	    };
4158	    assert!(zdr_video_output_s3.as_ref().is_some_and(|c| c.is_valid()));
4159	    assert!(!zdr_restricted);
4160	}
4161	#[tokio::test(flavor = "current_thread")]
4162	async fn prepare_video_gen_config_respects_feature_flag() {
4163	    use xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig;
4164	    let agent = build_minimal_agent_for_tests();
4165	    agent.sampling_config.borrow_mut().api_key = Some("test-key".to_string());
4166	    assert!(matches!(
4167	        agent.prepare_video_gen_config(),
4168	        VideoGenConfig::Enabled { .. }
4169	    ));
4170	    agent.cfg.borrow_mut().features.video_gen = Some(false);
4171	    assert!(matches!(
4172	        agent.prepare_video_gen_config(),
4173	        VideoGenConfig::Disabled
4174	    ));
4175	}
4176	/// The imagine tier gate fails **open**: with no resolved auth we can't confirm a restricted personal tier. The tools stay advertised and un-flagged. The server 429 remains the authoritative backstop.
4177	/// Guards against accidentally disabling a paid feature when tier info hasn't loaded.
4178	#[tokio::test(flavor = "current_thread")]
4179	async fn prepare_image_gen_config_fails_open_without_auth() {
4180	    use xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig;
4181	    let agent = build_minimal_agent_for_tests();
4182	    agent.sampling_config.borrow_mut().api_key = Some("test-key".to_string());
4183	    let ImageGenConfig::Enabled {
4184	        tier_restricted, ..
4185	    } = agent.prepare_image_gen_config()
4186	    else {
4187	        panic!("expected Enabled");
4188	    };
4189	    assert!(
4190	        !tier_restricted,
4191	        "no resolved auth ⇒ fail open (tools not tier-restricted)"
4192	    );
4193	}
4194	/// The imagine tools bypass cli-chat-proxy (direct API calls).
4195	/// The server can only scope the coding data-retention opt-out (`/privacy opt-out`) to Build traffic via the `x-grok-client-identifier` header.
4196	/// If this header is dropped, opted-out users' imagine prompts are logged/retained server-side.
4197	#[tokio::test(flavor = "current_thread")]
4198	async fn prepare_image_gen_config_sends_client_identifier_header() {
4199	    use xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig;
4200	    let agent = build_minimal_agent_for_tests();
4201	    agent.sampling_config.borrow_mut().api_key = Some("test-key".to_string());
4202	    let ImageGenConfig::Enabled { extra_headers, .. } = agent.prepare_image_gen_config() else {
4203	        panic!("expected Enabled");
4204	    };
4205	    assert_eq!(
4206	        extra_headers
4207	            .get("x-grok-client-identifier")
4208	            .map(String::as_str),
4209	        Some(crate::http::process_client_identifier().as_str()),
4210	        "imagine API calls must carry the client identifier so the server \
4211	         applies the coding ZDR opt-out to Build traffic"
4212	    );
4213	}
4214	/// Same contract for video generation (also a direct API call).
4215	#[tokio::test(flavor = "current_thread")]
4216	async fn prepare_video_gen_config_sends_client_identifier_header() {
4217	    use xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig;
4218	    let agent = build_minimal_agent_for_tests();
4219	    agent.sampling_config.borrow_mut().api_key = Some("test-key".to_string());
4220	    let VideoGenConfig::Enabled { extra_headers, .. } = agent.prepare_video_gen_config() else {
4221	        panic!("expected Enabled");
4222	    };
4223	    assert_eq!(
4224	        extra_headers
4225	            .get("x-grok-client-identifier")
4226	            .map(String::as_str),
4227	        Some(crate::http::process_client_identifier().as_str()),
4228	        "video gen API calls must carry the client identifier so the server \
4229	         applies the coding ZDR opt-out to Build traffic"
4230	    );
4231	}
4232	/// Regression: `x.ai/auth/info` must return profile fields even when the access token is expired.
4233	/// Profile data does not expire with the token, and hiding it made the desktop render "Signed in" with no identity.
4234	#[tokio::test]
4235	async fn auth_info_returns_profile_when_token_expired() {
4236	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth {
4237	        email: Some("user@example.com".into()),
4238	        first_name: Some("Test".into()),
4239	        refresh_token: Some("rt".into()),
4240	        expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
4241	        ..xai_grok_login::GrokAuth::test_default()
4242	    });
4243	    let resp = crate::extensions::auth::handle(
4244	        &agent,
4245	        &acp::ExtRequest::new(
4246	            "x.ai/auth/info",
4247	            std::sync::Arc::from(serde_json::value::to_raw_value(&serde_json::json!({})).unwrap()),
4248	        ),
4249	    )
4250	    .await
4251	    .expect("auth/info must succeed with an expired token");
4252	    let info: serde_json::Value = serde_json::from_str(resp.0.get()).unwrap();
4253	    assert_eq!(
4254	        info.get("email").and_then(|v| v.as_str()),
4255	        Some("user@example.com")
4256	    );
4257	    assert_eq!(info.get("firstName").and_then(|v| v.as_str()), Some("Test"));
4258	}
4259	#[tokio::test]
4260	async fn data_collection_enabled_for_normal_user() {
4261	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth::test_default());
4262	    if xai_grok_version::research_data_collection_forbidden() {
4263	        assert!(
4264	            agent.is_data_collection_disabled(),
4265	            "privacy build: research collection is always disabled"
4266	        );
4267	    } else {
4268	        assert!(
4269	            !agent.is_data_collection_disabled(),
4270	            "normal user must have data collection enabled"
4271	        );
4272	    }
4273	}
4274	#[tokio::test]
4275	async fn data_collection_disabled_for_zdr_team() {
4276	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth {
4277	        team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS".into()],
4278	        ..xai_grok_login::GrokAuth::test_default()
4279	    });
4280	    assert!(
4281	        agent.is_data_collection_disabled(),
4282	        "ZDR team must have data collection disabled"
4283	    );
4284	    assert!(
4285	        agent.trace_upload_config_snapshot().is_none(),
4286	        "trace uploads must be disabled for ZDR team"
4287	    );
4288	}
4289	#[tokio::test]
4290	async fn data_collection_disabled_for_zdr_moderated_team() {
4291	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth {
4292	        team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS_MODERATED".into()],
4293	        ..xai_grok_login::GrokAuth::test_default()
4294	    });
4295	    assert!(
4296	        agent.is_data_collection_disabled(),
4297	        "ZDR-moderated team must have data collection disabled"
4298	    );
4299	}
4300	#[tokio::test]
4301	async fn data_collection_disabled_for_opted_out_team() {
4302	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth {
4303	        coding_data_retention_opt_out: true,
4304	        ..xai_grok_login::GrokAuth::test_default()
4305	    });
4306	    assert!(
4307	        agent.is_data_collection_disabled(),
4308	        "opted-out team must have data collection disabled"
4309	    );
4310	    assert!(
4311	        agent.trace_upload_config_snapshot().is_none(),
4312	        "trace uploads must be disabled for opted-out team"
4313	    );
4314	}
4315	#[tokio::test]
4316	async fn data_collection_disabled_for_zdr_plus_opt_out() {
4317	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth {
4318	        team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS".into()],
4319	        coding_data_retention_opt_out: true,
4320	        ..xai_grok_login::GrokAuth::test_default()
4321	    });
4322	    assert!(
4323	        agent.is_data_collection_disabled(),
4324	        "ZDR + opt-out must have data collection disabled"
4325	    );
4326	}
4327	#[tokio::test]
4328	async fn data_collection_enabled_for_non_zdr_team_with_unrelated_blocks() {
4329	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth {
4330	        team_blocked_reasons: vec![
4331	            "BLOCKED_REASON_BILLING".into(),
4332	            "BLOCKED_REASON_SUSPENDED".into(),
4333	        ],
4334	        ..xai_grok_login::GrokAuth::test_default()
4335	    });
4336	    if xai_grok_version::research_data_collection_forbidden() {
4337	        assert!(
4338	            agent.is_data_collection_disabled(),
4339	            "privacy build: research collection is always disabled"
4340	        );
4341	    } else {
4342	        assert!(
4343	            !agent.is_data_collection_disabled(),
4344	            "non-ZDR blocked reasons must not disable data collection"
4345	        );
4346	    }
4347	}
4348	fn enable_product_telemetry(agent: &MvpAgent) {
4349	    agent.cfg.borrow_mut().features.telemetry = Some(crate::agent::config::TelemetryMode::Enabled);
4350	}
4351	/// Enable trace uploads via config so only the auth-level privacy gate can disable collection in the tests below.
4352	fn enable_trace_upload_config(agent: &MvpAgent) {
4353	    let mut cfg = agent.cfg.borrow_mut();
4354	    cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Enabled);
4355	    cfg.telemetry.trace_upload = Some(true);
4356	}
4357	#[tokio::test]
4358	async fn product_analytics_enabled_for_normal_user_with_telemetry_on() {
4359	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth::test_default());
4360	    enable_product_telemetry(&agent);
4361	    if xai_grok_version::research_data_collection_forbidden() {
4362	        assert!(
4363	            !agent.product_analytics_enabled(),
4364	            "privacy build: product analytics is always off"
4365	        );
4366	    } else {
4367	        assert!(agent.product_analytics_enabled());
4368	    }
4369	}
4370	#[tokio::test]
4371	async fn product_analytics_enabled_despite_coding_retention_opt_out() {
4372	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth {
4373	        coding_data_retention_opt_out: true,
4374	        ..xai_grok_login::GrokAuth::test_default()
4375	    });
4376	    enable_product_telemetry(&agent);
4377	    assert!(agent.is_data_collection_disabled());
4378	    if xai_grok_version::research_data_collection_forbidden() {
4379	        assert!(
4380	            !agent.product_analytics_enabled(),
4381	            "privacy build: product analytics is always off"
4382	        );
4383	    } else {
4384	        assert!(agent.product_analytics_enabled());
4385	    }
4386	}
4387	#[tokio::test]
4388	async fn product_analytics_disabled_for_zdr_team() {
4389	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth {
4390	        team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS".into()],
4391	        ..xai_grok_login::GrokAuth::test_default()
4392	    });
4393	    enable_product_telemetry(&agent);
4394	    assert!(!agent.product_analytics_enabled());
4395	}
4396	#[tokio::test]
4397	async fn product_analytics_disabled_when_telemetry_off() {
4398	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth::test_default());
4399	    agent.cfg.borrow_mut().features.telemetry = Some(crate::agent::config::TelemetryMode::Disabled);
4400	    assert!(!agent.product_analytics_enabled());
4401	}
4402	/// Counting HTTP stub: any request increments the counter and gets a storage-proxy-shaped 200 so the client does not retry.
4403	async fn spawn_counting_storage_stub() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
4404	    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
4405	    let count_clone = count.clone();
4406	    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
4407	    let port = listener.local_addr().unwrap().port();
4408	    let app = axum::Router::new().fallback(move || {
4409	        let count = count_clone.clone();
4410	        async move {
4411	            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
4412	            (
4413	                [("content-type", "application/json")],
4414	                r#"{"bucket":"test-bucket","path":"auth-diagnostics/test.jsonl"}"#,
4415	            )
4416	        }
4417	    });
4418	    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
4419	    (format!("http://127.0.0.1:{port}"), count)
4420	}
4421	/// Regression: the auth-diagnostics uploader was gated only on the trace-upload config switch.
4422	/// It must also honor ZDR / retention opt-out, checked at invocation time.
4423	#[tokio::test]
4424	async fn diagnostic_upload_skipped_for_opted_out_user() {
4425	    let (stub_url, count) = spawn_counting_storage_stub().await;
4426	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth {
4427	        coding_data_retention_opt_out: true,
4428	        ..xai_grok_login::GrokAuth::test_default()
4429	    });
4430	    enable_trace_upload_config(&agent);
4431	    agent.cfg.borrow_mut().endpoints.trace_upload_url = Some(stub_url);
4432	    if xai_grok_version::research_data_collection_forbidden() {
4433	        // Privacy build: trace upload is hard-off, so no uploader is wired.
4434	        assert!(
4435	            agent.diagnostic_upload_config().is_none(),
4436	            "privacy build: diagnostics upload must never be wired"
4437	        );
4438	        return;
4439	    }
4440	    let uploader = agent
4441	        .diagnostic_upload_config()
4442	        .expect("uploader is wired whenever trace upload config is on");
4443	    uploader(b"log".to_vec(), "tok".into(), "user-id-1".into()).await;
4444	    assert_eq!(
4445	        count.load(std::sync::atomic::Ordering::SeqCst),
4446	        0,
4447	        "no diagnostics request may leave the machine after opt-out"
4448	    );
4449	}
4450	#[tokio::test]
4451	async fn diagnostic_upload_sent_for_normal_user() {
4452	    let (stub_url, count) = spawn_counting_storage_stub().await;
4453	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth::test_default());
4454	    enable_trace_upload_config(&agent);
4455	    agent.cfg.borrow_mut().endpoints.trace_upload_url = Some(stub_url);
4456	    if xai_grok_version::research_data_collection_forbidden() {
4457	        // Privacy build: trace upload is hard-off, so no uploader is wired.
4458	        assert!(
4459	            agent.diagnostic_upload_config().is_none(),
4460	            "privacy build: diagnostics upload must never be wired"
4461	        );
4462	        return;
4463	    }
4464	    let uploader = agent
4465	        .diagnostic_upload_config()
4466	        .expect("uploader is wired whenever trace upload config is on");
4467	    uploader(b"log".to_vec(), "tok".into(), "user-id-1".into()).await;
4468	    assert!(
4469	        count.load(std::sync::atomic::Ordering::SeqCst) >= 1,
4470	        "positive control: diagnostics upload reaches the proxy for a \
4471	         normal user"
4472	    );
4473	}
4474	/// The diagnostics privacy gate fails closed: with no credential in the `AuthManager`, nothing may leave the machine.
4475	/// The credential can be missing when a mid-session `/logout` raced the refresh failure that triggers the upload.
4476	#[tokio::test]
4477	async fn diagnostic_upload_skipped_without_credentials() {
4478	    let (stub_url, count) = spawn_counting_storage_stub().await;
4479	    let agent = build_minimal_agent_for_tests();
4480	    enable_trace_upload_config(&agent);
4481	    agent.cfg.borrow_mut().endpoints.trace_upload_url = Some(stub_url);
4482	    if xai_grok_version::research_data_collection_forbidden() {
4483	        // Privacy build: trace upload is hard-off, so no uploader is wired.
4484	        assert!(
4485	            agent.diagnostic_upload_config().is_none(),
4486	            "privacy build: diagnostics upload must never be wired"
4487	        );
4488	        return;
4489	    }
4490	    let uploader = agent
4491	        .diagnostic_upload_config()
4492	        .expect("uploader is wired whenever trace upload config is on");
4493	    uploader(b"log".to_vec(), "tok".into(), "user-id-1".into()).await;
4494	    assert_eq!(
4495	        count.load(std::sync::atomic::Ordering::SeqCst),
4496	        0,
4497	        "missing credentials must fail closed for diagnostics uploads"
4498	    );
4499	}
4500	/// The coding-data opt-out governs sharing with xAI, so traces still go to a deployment's own bucket.
4501	/// Everything else the opt-out gates stays closed.
4502	#[tokio::test]
4503	async fn opted_out_user_uploads_traces_only_to_own_bucket() {
4504	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth {
4505	        coding_data_retention_opt_out: true,
4506	        ..xai_grok_login::GrokAuth::test_default()
4507	    });
4508	    enable_trace_upload_config(&agent);
4509	    agent.cfg.borrow_mut().remote_settings = Some(crate::util::config::RemoteSettings::default());
4510	    assert!(
4511	        agent.trace_upload_config_snapshot().is_none(),
4512	        "precondition: no own bucket, so the opt-out blocks uploads"
4513	    );
4514	    if xai_grok_version::research_data_collection_forbidden() {
4515	        agent.cfg.borrow_mut().endpoints.trace_upload_bucket = Some("s3://acme-traces".into());
4516	        assert!(
4517	            agent.trace_upload_config_snapshot().is_none(),
4518	            "privacy hard-off must keep own-bucket uploads closed"
4519	        );
4520	        assert!(agent.is_data_collection_disabled());
4521	        return;
4522	    }
4523	    agent.cfg.borrow_mut().endpoints.trace_upload_bucket = Some("s3://acme-traces".into());
4524	    let is_own_bucket = |method: Option<crate::session::repo_changes::UploadMethod>| {
4525	        matches!(
4526	            method,
4527	            Some(crate::session::repo_changes::UploadMethod::S3 { bucket, .. }) if bucket == "acme-traces"
4528	        )
4529	    };
4530	    assert!(is_own_bucket(agent.trace_upload_config_snapshot()));
4531	    assert!(is_own_bucket(agent.trace_upload_config().await));
4532	    assert!(
4533	        agent.is_data_collection_disabled(),
4534	        "heap profiles, workspace snapshots, and diagnostics still see the opt-out"
4535	    );
4536	}
4537	#[tokio::test]
4538	async fn zdr_team_uploads_no_traces_to_own_bucket() {
4539	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth {
4540	        team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS".into()],
4541	        ..xai_grok_login::GrokAuth::test_default()
4542	    });
4543	    enable_trace_upload_config(&agent);
4544	    agent.cfg.borrow_mut().endpoints.trace_upload_bucket = Some("s3://acme-traces".into());
4545	    assert!(agent.trace_upload_config_snapshot().is_none());
4546	}
4547	/// Auth diagnostics always go to the proxy, so a deployment's own bucket must not open them for an opted-out user.
4548	#[tokio::test]
4549	async fn diagnostic_upload_skipped_for_opted_out_user_with_own_bucket() {
4550	    let (stub_url, count) = spawn_counting_storage_stub().await;
4551	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth {
4552	        coding_data_retention_opt_out: true,
4553	        ..xai_grok_login::GrokAuth::test_default()
4554	    });
4555	    enable_trace_upload_config(&agent);
4556	    {
4557	        let mut cfg = agent.cfg.borrow_mut();
4558	        cfg.endpoints.trace_upload_url = Some(stub_url);
4559	        cfg.endpoints.trace_upload_bucket = Some("s3://acme-traces".into());
4560	    }
4561	    if xai_grok_version::research_data_collection_forbidden() {
4562	        // Privacy build: trace upload is hard-off, so no uploader is wired.
4563	        assert!(
4564	            agent.diagnostic_upload_config().is_none(),
4565	            "privacy build: diagnostics upload must never be wired"
4566	        );
4567	        return;
4568	    }
4569	    let uploader = agent
4570	        .diagnostic_upload_config()
4571	        .expect("uploader is wired whenever trace upload config is on");
4572	    uploader(b"log".to_vec(), "tok".into(), "user-id-1".into()).await;
4573	    assert_eq!(0, count.load(std::sync::atomic::Ordering::SeqCst));
4574	}
4575	/// The diagnostics uploader is wired once (at agent construction), so it must re-check the live trace-upload mirror at invocation time.
4576	/// A mid-session config-level kill switch stops diagnostics uploads too.
4577	#[tokio::test]
4578	async fn diagnostic_upload_skipped_after_mid_session_trace_upload_kill_switch() {
4579	    let (stub_url, count) = spawn_counting_storage_stub().await;
4580	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth::test_default());
4581	    enable_trace_upload_config(&agent);
4582	    agent.cfg.borrow_mut().endpoints.trace_upload_url = Some(stub_url);
4583	    agent.sync_collection_config_gate();
4584	    if xai_grok_version::research_data_collection_forbidden() {
4585	        // Privacy build: trace upload is hard-off, so no uploader is wired.
4586	        assert!(
4587	            agent.diagnostic_upload_config().is_none(),
4588	            "privacy build: diagnostics upload must never be wired"
4589	        );
4590	        return;
4591	    }
4592	    let uploader = agent
4593	        .diagnostic_upload_config()
4594	        .expect("uploader is wired whenever trace upload config is on");
4595	    {
4596	        let mut cfg = agent.cfg.borrow_mut();
4597	        cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Disabled);
4598	        cfg.telemetry.trace_upload = Some(false);
4599	    }
4600	    agent.sync_collection_config_gate();
4601	    uploader(b"log".to_vec(), "tok".into(), "user-id-1".into()).await;
4602	    assert_eq!(
4603	        count.load(std::sync::atomic::Ordering::SeqCst),
4604	        0,
4605	        "an already-wired diagnostics uploader must honor a mid-session \
4606	         trace-upload kill switch"
4607	    );
4608	}
4609	use crate::session::storage::search::IndexDecision;
4610	/// A grok home of its own, with the switch left at its registered default.
4611	/// `decide_search_index` stops short of a session store, but do not reach `bootstrap_once`.
4612	/// `bootstrap_once` takes the process-cached `grok_home()`, which these guards cannot redirect, so it could index the developer's own store.
4613	fn search_index_env() -> (tempfile::TempDir, [xai_grok_test_support::EnvGuard; 2]) {
4614	    use xai_grok_test_support::EnvGuard;
4615	    let home = tempfile::tempdir().unwrap();
4616	    let guards = [
4617	        EnvGuard::set("GROK_HOME", home.path()),
4618	        EnvGuard::unset("GROK_SESSION_SEARCH"),
4619	    ];
4620	    (home, guards)
4621	}
4622	#[tokio::test]
4623	#[serial_test::serial]
4624	async fn search_index_honors_the_session_search_feature() {
4625	    let (_home, _env) = search_index_env();
4626	    {
4627	        let _off = xai_grok_test_support::EnvGuard::set("GROK_SESSION_SEARCH", "0");
4628	        let agent = build_agent_with_auth(xai_grok_login::GrokAuth::test_default());
4629	        agent.decide_search_index();
4630	        assert!(
4631	            matches!(agent.search_index(), IndexDecision::Off),
4632	            "the switch is off, so this process keeps no index"
4633	        );
4634	    }
4635	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth::test_default());
4636	    agent.decide_search_index();
4637	    assert!(
4638	        matches!(agent.search_index(), IndexDecision::On(_)),
4639	        "the feature is on by default, so this process keeps an index"
4640	    );
4641	}
4642	/// Reclaiming is the one irreversible half of the deferred work, and the six hour throttle then hides the run that could have honored a remote veto.
4643	#[tokio::test]
4644	#[serial_test::serial]
4645	async fn auto_gc_declines_until_the_remote_answer_settles() {
4646	    let (_home, _env) = search_index_env();
4647	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth::test_default());
4648	    assert!(
4649	        !agent.remote_settings_settled(),
4650	        "precondition: remote fetch is on and no settings have arrived"
4651	    );
4652	    agent.spawn_auto_worktree_gc();
4653	    agent.decide_search_index();
4654	    assert_eq!(
4655	        agent.auto_gc_spawn_count.get(),
4656	        0,
4657	        "a host that never reaches the server must not reclaim under the default policy"
4658	    );
4659	    assert!(
4660	        matches!(agent.search_index(), IndexDecision::On(_)),
4661	        "the index still decides, since running without one is what cannot be undone; got {:?}",
4662	        agent.search_index(),
4663	    );
4664	    agent.cfg.borrow_mut().remote_settings = Some(crate::util::config::RemoteSettings::default());
4665	    agent.spawn_auto_worktree_gc();
4666	    assert_eq!(
4667	        agent.auto_gc_spawn_count.get(),
4668	        1,
4669	        "once the server has answered it reclaims, or the guard would just never run"
4670	    );
4671	}
4672	#[tokio::test]
4673	#[serial_test::serial]
4674	async fn search_before_the_decision_asks_the_caller_to_retry() {
4675	    let (_home, _env) = search_index_env();
4676	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth::test_default());
4677	    assert!(
4678	        matches!(agent.search_index(), IndexDecision::Pending),
4679	        "precondition: nothing has decided yet"
4680	    );
4681	    let resp = crate::session::storage::search::execute_search(
4682	        agent.search_index(),
4683	        &crate::util::grok_home::grok_home(),
4684	        &crate::session::storage::search::SessionSearchRequest {
4685	            query: "zzqqpending".to_string(),
4686	            cwd: None,
4687	            limit: 10,
4688	            offset: 0,
4689	            include_content: false,
4690	        },
4691	    )
4692	    .await
4693	    .unwrap();
4694	    assert!(
4695	        resp.bootstrapping,
4696	        "an undecided process must not answer final"
4697	    );
4698	    assert!(resp.results.is_empty());
4699	}
4700	/// A leader boots with no remote settings.
4701	/// If the first reader resolved the feature, the registered default would latch before the server could answer.
4702	#[tokio::test]
4703	#[serial_test::serial]
4704	async fn read_before_the_remote_settings_land_does_not_decide() {
4705	    let (_home, _env) = search_index_env();
4706	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth::test_default());
4707	    assert!(
4708	        !agent.remote_settings_settled(),
4709	        "precondition: remote fetch is on and no settings have arrived"
4710	    );
4711	    assert!(
4712	        matches!(agent.search_index(), IndexDecision::Pending),
4713	        "reading must not settle the question; got {:?}",
4714	        agent.search_index(),
4715	    );
4716	    agent.cfg.borrow_mut().remote_settings = Some(crate::util::config::RemoteSettings {
4717	        session_search: Some(false),
4718	        ..Default::default()
4719	    });
4720	    assert!(agent.remote_settings_settled());
4721	    agent.decide_search_index();
4722	    assert!(
4723	        matches!(agent.search_index(), IndexDecision::Off),
4724	        "the remote kill switch decided, so this process keeps no index; got {:?}",
4725	        agent.search_index(),
4726	    );
4727	}
4728	/// A host with no identity to fetch with never receives remote settings, so waiting for them would cost it an index for the whole run.
4729	#[tokio::test]
4730	#[serial_test::serial]
4731	async fn exhausted_fetch_decides_on_the_local_layers() {
4732	    use crate::agent::config::Config as AgentConfig;
4733	    use xai_grok_login::{AuthManager, GrokComConfig};
4734	    use xai_grok_test_support::EnvGuard;
4735	    let (_home, _env) = search_index_env();
4736	    let _no_inline_auth = EnvGuard::unset("GROK_AUTH");
4737	    let _no_auth_path = EnvGuard::unset("GROK_AUTH_PATH");
4738	    let auth_dir = tempfile::tempdir().unwrap();
4739	    let auth_manager =
4740	        std::sync::Arc::new(AuthManager::new(auth_dir.path(), GrokComConfig::default()));
4741	    assert!(
4742	        auth_manager.current().is_none(),
4743	        "precondition: no identity to fetch with"
4744	    );
4745	    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
4746	    let agent = MvpAgent::new(
4747	        GatewaySender::new(tx),
4748	        &AgentConfig::default(),
4749	        auth_manager,
4750	        None,
4751	        None,
4752	    )
4753	    .expect("valid test config");
4754	    assert!(
4755	        !agent.remote_settings_settled(),
4756	        "precondition: remote fetch is on and no settings have arrived"
4757	    );
4758	    agent.maybe_fetch_post_auth_settings().await;
4759	    assert!(
4760	        matches!(agent.search_index(), IndexDecision::On(_)),
4761	        "a signed out host decides on its local layers rather than keeping no index"
4762	    );
4763	}
4764	/// Once decided, a later switch cannot take the index away: serving one that stopped absorbing writes is worse than keeping it until the next launch.
4765	#[tokio::test]
4766	#[serial_test::serial]
4767	async fn kill_switch_after_the_decision_leaves_the_index_up() {
4768	    let (_home, _env) = search_index_env();
4769	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth::test_default());
4770	    agent.cfg.borrow_mut().remote_settings = Some(crate::util::config::RemoteSettings {
4771	        session_search: Some(true),
4772	        ..Default::default()
4773	    });
4774	    agent.decide_search_index();
4775	    assert!(
4776	        matches!(agent.search_index(), IndexDecision::On(_)),
4777	        "precondition: indexing"
4778	    );
4779	    agent.cfg.borrow_mut().remote_settings = Some(crate::util::config::RemoteSettings {
4780	        session_search: Some(false),
4781	        ..Default::default()
4782	    });
4783	    agent.decide_search_index();
4784	    assert!(
4785	        matches!(agent.search_index(), IndexDecision::On(_)),
4786	        "a switch arriving after the decision must not tear down a live index"
4787	    );
4788	}
4789	/// Sessions hold the decision itself, not the answer as it stood when they opened, which would otherwise be `None` for the rest of their life.
4790	#[tokio::test]
4791	#[serial_test::serial]
4792	async fn session_opened_before_the_decision_sees_it_land() {
4793	    let (_home, _env) = search_index_env();
4794	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth::test_default());
4795	    let held_by_a_session = agent.search_index_cell();
4796	    assert!(
4797	        matches!(held_by_a_session.decision(), IndexDecision::Pending),
4798	        "precondition: the session opened before anything decided"
4799	    );
4800	    agent.cfg.borrow_mut().remote_settings = Some(crate::util::config::RemoteSettings {
4801	        session_search: Some(true),
4802	        ..Default::default()
4803	    });
4804	    agent.decide_search_index();
4805	    assert!(
4806	        matches!(held_by_a_session.decision(), IndexDecision::On(_)),
4807	        "the session indexes as soon as the decision lands"
4808	    );
4809	}
4810	/// The live collection gate reads a `Send` mirror of the config-level trace-upload switch.
4811	/// `sync_collection_config_gate` must keep that mirror current.
4812	/// A mid-session remote-settings flip (kill switch) then stops collection without a new session.
4813	#[tokio::test]
4814	async fn collection_config_gate_mirror_follows_trace_upload_flip() {
4815	    let agent = build_agent_with_auth(xai_grok_login::GrokAuth::test_default());
4816	    enable_trace_upload_config(&agent);
4817	    agent.sync_collection_config_gate();
4818	    if xai_grok_version::research_data_collection_forbidden() {
4819	        // Privacy build: trace upload is hard-off, so the mirror starts off
4820	        // and a flip cannot turn it on.
4821	        assert!(
4822	            !agent
4823	                .trace_upload_live
4824	                .load(std::sync::atomic::Ordering::Relaxed),
4825	            "privacy build: mirror must start off"
4826	        );
4827	        {
4828	            let mut cfg = agent.cfg.borrow_mut();
4829	            cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Disabled);
4830	            cfg.telemetry.trace_upload = Some(false);
4831	        }
4832	        agent.sync_collection_config_gate();
4833	        assert!(
4834	            !agent
4835	                .trace_upload_live
4836	                .load(std::sync::atomic::Ordering::Relaxed),
4837	            "mirror must stay off under the privacy build"
4838	        );
4839	        return;
4840	    }
4841	    assert!(
4842	        agent
4843	            .trace_upload_live
4844	            .load(std::sync::atomic::Ordering::Relaxed),
4845	        "precondition: mirror reflects the enabled switch"
4846	    );
4847	    {
4848	        let mut cfg = agent.cfg.borrow_mut();
4849	        cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Disabled);
4850	        cfg.telemetry.trace_upload = Some(false);
4851	    }
4852	    agent.sync_collection_config_gate();
4853	    assert!(
4854	        !agent
4855	            .trace_upload_live
4856	            .load(std::sync::atomic::Ordering::Relaxed),
4857	        "mirror must follow a mid-session config-level trace-upload flip"
4858	    );
4859	}
4860	/// `parse_session_kind` routes `session/load` to the gateway Chat path vs. the disk-backed Build path.
4861	/// Anything but an explicit `kind: "chat"` is Build.
4862	#[test]
4863	fn parse_session_kind_matrix() {
4864	    use crate::session::unified_list::SessionKind;
4865	    use serde_json::json;
4866	    let cases: &[(&str, serde_json::Value, SessionKind)] = &[
4867	        (
4868	            "chat",
4869	            json!({"x.ai/session": {"kind": "chat"}}),
4870	            SessionKind::Chat,
4871	        ),
4872	        (
4873	            "build",
4874	            json!({"x.ai/session": {"kind": "build"}}),
4875	            SessionKind::Build,
4876	        ),
4877	        (
4878	            "chat_malformed_sibling",
4879	            json!({"x.ai/session": {"kind": "chat", "facets": "not-a-map"}}),
4880	            SessionKind::Chat,
4881	        ),
4882	        (
4883	            "unknown_kind",
4884	            json!({"x.ai/session": {"kind": "frob"}}),
4885	            SessionKind::Build,
4886	        ),
4887	        ("absent", json!({}), SessionKind::Build),
4888	    ];
4889	    for (label, meta, expected) in cases {
4890	        assert_eq!(parse_session_kind(meta.as_object()), *expected, "[{label}]");
4891	    }
4892	    assert_eq!(parse_session_kind(None), SessionKind::Build, "[none]");
4893	}
4894	#[test]
4895	fn reject_chat_kind_without_feature_errors_without_chat_feature() {
4896	    use serde_json::json;
4897	    assert!(
4898	        reject_chat_kind_without_feature(json!({"x.ai/session": {"kind": "chat"}}).as_object())
4899	            .is_err()
4900	    );
4901	    assert!(reject_chat_kind_without_feature(None).is_ok());
4902	    assert!(
4903	        reject_chat_kind_without_feature(
4904	            json!({ "x.ai/session" : { "kind" : "build" } }).as_object()
4905	        )
4906	        .is_ok()
4907	    );
4908	}
4909	#[test]
4910	fn chat_initial_model_matrix() {
4911	    let cases: &[(&str, bool, Option<&str>, Option<&str>)] = &[
4912	        ("chat_with_model", true, Some("grok-4.5"), Some("grok-4.5")),
4913	        ("chat_without_model", true, None, None),
4914	        ("build_with_model", false, Some("grok-4.5"), None),
4915	        ("build_without_model", false, None, None),
4916	    ];
4917	    for (label, is_chat_kind, custom_model_id, expected) in cases {
4918	        assert_eq!(
4919	            chat_initial_model(*is_chat_kind, *custom_model_id).as_deref(),
4920	            *expected,
4921	            "[{label}]"
4922	        );
4923	    }
4924	}
4925	#[test]
4926	fn chat_new_session_model_state_matrix() {
4927	    fn state_with(current: &str, available: &[&str]) -> acp::SessionModelState {
4928	        acp::SessionModelState::new(
4929	            acp::ModelId::new(current.to_owned()),
4930	            available
4931	                .iter()
4932	                .map(|id| {
4933	                    acp::ModelInfo::new(acp::ModelId::new((*id).to_owned()), (*id).to_owned())
4934	                })
4935	                .collect(),
4936	        )
4937	    }
4938	    let cases: &[(&str, acp::SessionModelState, Option<&str>, &str)] = &[
4939	        (
4940	            "requested_in_catalog",
4941	            state_with("auto", &["auto", "grok-4"]),
4942	            Some("grok-4"),
4943	            "grok-4",
4944	        ),
4945	        (
4946	            "no_request_keeps_catalog_default",
4947	            state_with("auto", &["auto", "grok-4"]),
4948	            None,
4949	            "auto",
4950	        ),
4951	        (
4952	            "requested_not_in_catalog",
4953	            state_with("auto", &["auto"]),
4954	            Some("grok-4.5"),
4955	            "grok-4.5",
4956	        ),
4957	        (
4958	            "requested_with_empty_catalog",
4959	            state_with("", &[]),
4960	            Some("grok-4"),
4961	            "grok-4",
4962	        ),
4963	    ];
4964	    for (label, state, requested, expected) in cases {
4965	        let out = chat_new_session_model_state(state.clone(), requested.map(str::to_owned));
4966	        assert_eq!(out.current_model_id.0.as_ref(), *expected, "[{label}]");
4967	        assert_eq!(
4968	            out.available_models.len(),
4969	            state.available_models.len(),
4970	            "[{label}] override must not mutate the catalog"
4971	        );
4972	    }
4973	}
4974	/// A valid `x.ai/local_workspace` parses to ExistingWorkspace only.
4975	/// It never reads `envId` and never emits SandboxEnvironment.
4976	#[cfg(feature = "local-workspace")]
4977	#[test]
4978	fn parse_session_computer_sessions_local_workspace_matrix() {
4979	    use crate::gateway_bridge::ComputerSession;
4980	    use serde_json::json;
4981	    fn existing(server_id: &str, cwd: Option<&str>) -> Vec<ComputerSession> {
4982	        vec![ComputerSession::ExistingWorkspace {
4983	            server_id: server_id.to_owned(),
4984	            cwd: cwd.map(str::to_owned),
4985	        }]
4986	    }
4987	    let cases: &[(&str, serde_json::Value, Option<Vec<ComputerSession>>)] = &[
4988	        (
4989	            "attach_server_id_on_local",
4990	            json!({
4991	                "x.ai/local_workspace": {
4992	                    "mode": "attach",
4993	                    "server_id": "lw-attach-1",
4994	                    "cwd": "/repo",
4995	                },
4996	                "envId": "env-must-be-ignored",
4997	            }),
4998	            Some(existing("lw-attach-1", Some("/repo"))),
4999	        ),
5000	        (
5001	            "attach_server_id_from_cloud_existing",
5002	            json!({
5003	                "x.ai/local_workspace": {
5004	                    "mode": "attach",
5005	                    "cwd": "/repo",
5006	                },
5007	                "x.ai/cloud_existing_workspace": {
5008	                    "server_id": "lw-attach-2",
5009	                    "cwd": "/repo-existing",
5010	                },
5011	                "envId": "env-must-be-ignored",
5012	            }),
5013	            Some(existing("lw-attach-2", Some("/repo"))),
5014	        ),
5015	        (
5016	            "own_with_server_id_ignores_envid",
5017	            json!({
5018	                "x.ai/local_workspace": {
5019	                    "mode": "own",
5020	                    "server_id": "lw-own-1",
5021	                    "cwd": "/Users/me/src",
5022	                },
5023	                "envId": "env-must-be-ignored",
5024	            }),
5025	            Some(existing("lw-own-1", Some("/Users/me/src"))),
5026	        ),
5027	        (
5028	            "own_without_server_id_no_sandbox_fallback",
5029	            json!({
5030	                "x.ai/local_workspace": {
5031	                    "mode": "own",
5032	                    "cwd": "/Users/me/src",
5033	                },
5034	                "envId": "env-must-be-ignored",
5035	            }),
5036	            None,
5037	        ),
5038	        (
5039	            "invalid_mode_falls_through_to_envid",
5040	            json!({
5041	                "x.ai/local_workspace": {
5042	                    "mode": "bogus",
5043	                    "server_id": "lw-x",
5044	                },
5045	                "envId": "env-prod",
5046	            }),
5047	            Some(vec![ComputerSession::SandboxEnvironment {
5048	                environment_id: Some("env-prod".to_owned()),
5049	            }]),
5050	        ),
5051	        (
5052	            "non_object_local_falls_through_to_envid",
5053	            json!({
5054	                "x.ai/local_workspace": "not-an-object",
5055	                "envId": "env-prod",
5056	            }),
5057	            Some(vec![ComputerSession::SandboxEnvironment {
5058	                environment_id: Some("env-prod".to_owned()),
5059	            }]),
5060	        ),
5061	    ];
5062	    for (label, meta, expected) in cases {
5063	        let got = parse_session_computer_sessions(meta.as_object());
5064	        assert_eq!(
5065	            got.as_deref(),
5066	            expected.as_deref(),
5067	            "[{label}] local_workspace match-table mismatch"
5068	        );
5069	    }
5070	}
5071	/// Local intent without resolvable server_id fails closed (no silent unstamped start).
5072	#[cfg(feature = "local-workspace")]
5073	#[test]
5074	fn resolve_local_workspace_missing_server_id_fails_closed() {
5075	    use serde_json::json;
5076	    let meta = json!({
5077	        "x.ai/session": { "kind": "chat" },
5078	        "x.ai/local_workspace": {
5079	            "mode": "own",
5080	            "cwd": "/repo",
5081	        }
5082	    });
5083	    let err = resolve_session_computer_sessions(meta.as_object())
5084	        .expect_err("own without server_id must fail closed");
5085	    assert_eq!(
5086	        err.data
5087	            .as_ref()
5088	            .and_then(|d| d.get("code"))
5089	            .and_then(|v| v.as_str()),
5090	        Some("local_workspace_server_id_missing")
5091	    );
5092	}
5093	/// The supervisor map, the reap guard, and shutdown_gateway_bridge tear down the entry.
5094	#[cfg(all(feature = "local-workspace", unix))]
5095	#[test]
5096	fn local_workspace_reap_guard_and_shutdown_clear_map() {
5097	    run_local_for_bridge_test(|| async {
5098	        let agent = build_minimal_agent_for_tests();
5099	        let sid = gateway_bridge_test_session_id();
5100	        {
5101	            let mut guard = agent.new_local_workspace_reap_guard(sid.clone(), true);
5102	            guard.disarm();
5103	        }
5104	        assert!(agent.local_workspace_supervisors.borrow().is_empty());
5105	        agent.shutdown_gateway_bridge(&sid);
5106	        assert!(
5107	            agent
5108	                .local_workspace_generations
5109	                .borrow()
5110	                .get(&sid)
5111	                .is_none()
5112	        );
5113	    });
5114	}
5115	/// A pre-bridge crash refresh rewrites the handshake stamp from the live supervisor id.
5116	#[cfg(all(feature = "local-workspace", unix))]
5117	#[test]
5118	fn refresh_sessions_from_supervisor_overrides_server_id() {
5119	    use crate::gateway_bridge::ComputerSession;
5120	    use crate::gateway_bridge::local_workspace_supervisor::test_start_ready_own;
5121	    run_local_for_bridge_test(|| async {
5122	        let agent = build_minimal_agent_for_tests();
5123	        let sid = gateway_bridge_test_session_id();
5124	        let original = Some(vec![ComputerSession::ExistingWorkspace {
5125	            server_id: "lw-stale".into(),
5126	            cwd: Some("/repo".into()),
5127	        }]);
5128	        let unchanged = agent.refresh_sessions_from_supervisor(&sid, original.clone());
5129	        assert!(matches!(
5130	            unchanged.as_ref().and_then(|v| v.first()),
5131	            Some(ComputerSession::ExistingWorkspace { server_id, .. }) if server_id == "lw-stale"
5132	        ));
5133	        let (_dir, handle) = test_start_ready_own().await;
5134	        let live_id = handle.server_id.clone();
5135	        agent.register_local_workspace_supervisor(sid.clone(), handle);
5136	        let refreshed = agent.refresh_sessions_from_supervisor(&sid, original);
5137	        match refreshed.as_ref().and_then(|v| v.first()) {
5138	            Some(ComputerSession::ExistingWorkspace { server_id, .. }) => {
5139	                assert_eq!(
5140	                    server_id, &live_id,
5141	                    "refresh must use live supervisor server_id"
5142	                );
5143	            }
5144	            other => panic!("expected ExistingWorkspace, got {other:?}"),
5145	        }
5146	        agent.shutdown_gateway_bridge(&sid);
5147	    });
5148	}
5149	/// start_own followed by register stamps server_id into meta and stores the handle.
5150	#[cfg(all(feature = "local-workspace", unix))]
5151	#[test]
5152	fn start_own_registers_and_stamps_server_id() {
5153	    use crate::gateway_bridge::local_workspace_supervisor::{
5154	        stamp_server_id_into_meta, test_start_ready_own,
5155	    };
5156	    run_local_for_bridge_test(|| async {
5157	        let agent = build_minimal_agent_for_tests();
5158	        let sid = gateway_bridge_test_session_id();
5159	        let (_dir, handle) = test_start_ready_own().await;
5160	        let server_id = handle.server_id.clone();
5161	        let mut meta = acp::Meta::new();
5162	        meta.insert(
5163	            "x.ai/local_workspace".into(),
5164	            serde_json::json!({"mode": "own", "cwd": "/tmp/repo"}),
5165	        );
5166	        stamp_server_id_into_meta(&mut meta, &server_id);
5167	        assert_eq!(
5168	            meta.get("x.ai/local_workspace")
5169	                .and_then(|v| v.get("server_id"))
5170	                .and_then(|v| v.as_str()),
5171	            Some(server_id.as_str())
5172	        );
5173	        agent.register_local_workspace_supervisor(sid.clone(), handle);
5174	        assert!(
5175	            agent
5176	                .local_workspace_supervisors
5177	                .borrow()
5178	                .contains_key(&sid),
5179	            "handle must be registered by SessionId"
5180	        );
5181	        assert!(
5182	            agent
5183	                .local_workspace_generations
5184	                .borrow()
5185	                .get(&sid)
5186	                .is_some_and(|g| *g >= 1),
5187	            "arm must bump generation"
5188	        );
5189	        agent.shutdown_gateway_bridge(&sid);
5190	    });
5191	}
5192	/// Armed reap guard removes a registered supervisor on drop (session/new failure).
5193	#[cfg(all(feature = "local-workspace", unix))]
5194	#[test]
5195	fn reap_guard_drop_removes_registered_supervisor() {
5196	    use crate::gateway_bridge::local_workspace_supervisor::test_start_ready_own;
5197	    run_local_for_bridge_test(|| async {
5198	        let agent = build_minimal_agent_for_tests();
5199	        let sid = gateway_bridge_test_session_id();
5200	        let (_dir, handle) = test_start_ready_own().await;
5201	        agent.register_local_workspace_supervisor(sid.clone(), handle);
5202	        assert!(
5203	            agent
5204	                .local_workspace_supervisors
5205	                .borrow()
5206	                .contains_key(&sid)
5207	        );
5208	        {
5209	            let _guard = agent.new_local_workspace_reap_guard(sid.clone(), true);
5210	        }
5211	        assert!(
5212	            agent
5213	                .local_workspace_supervisors
5214	                .borrow()
5215	                .get(&sid)
5216	                .is_none(),
5217	            "armed guard drop must reap supervisor"
5218	        );
5219	        assert!(
5220	            agent
5221	                .local_workspace_generations
5222	                .borrow()
5223	                .get(&sid)
5224	                .is_none(),
5225	            "armed guard drop must invalidate generation"
5226	        );
5227	    });
5228	}
5229	/// Shutdown generation invalidates a pending restart re-insert.
5230	#[cfg(all(feature = "local-workspace", unix))]
5231	#[test]
5232	fn shutdown_generation_invalidates_stale_restart() {
5233	    use crate::gateway_bridge::local_workspace_supervisor::test_start_ready_own;
5234	    run_local_for_bridge_test(|| async {
5235	        let agent = build_minimal_agent_for_tests();
5236	        let sid = gateway_bridge_test_session_id();
5237	        let (_dir, handle) = test_start_ready_own().await;
5238	        agent.register_local_workspace_supervisor(sid.clone(), handle);
5239	        let generation = *agent
5240	            .local_workspace_generations
5241	            .borrow()
5242	            .get(&sid)
5243	            .expect("generation after register");
5244	        agent.shutdown_gateway_bridge(&sid);
5245	        assert!(
5246	            agent.local_workspace_generations.borrow().get(&sid) != Some(&generation),
5247	            "shutdown must invalidate generation so stale restart cannot re-insert"
5248	        );
5249	        assert!(
5250	            agent
5251	                .local_workspace_supervisors
5252	                .borrow()
5253	                .get(&sid)
5254	                .is_none()
5255	        );
5256	    });
5257	}
5258	/// `spawn_gateway_bridge` uses `tokio::task::spawn_local`.
5259	fn run_local_for_bridge_test<F, Fut, T>(body: F) -> T
5260	where
5261	    F: FnOnce() -> Fut,
5262	    Fut: std::future::Future<Output = T>,
5263	{
5264	    let rt = tokio::runtime::Builder::new_current_thread()
5265	        .enable_all()
5266	        .build()
5267	        .expect("test runtime must build");
5268	    let local = tokio::task::LocalSet::new();
5269	    local.block_on(&rt, body())
5270	}
5271	#[test]
5272	fn chat_session_spawn_options_matches_thin_profile() {
5273	    let sid = acp::SessionId::new(std::sync::Arc::from("00000000-0000-0000-0000-000000000099"));
5274	    let cwd = xai_grok_paths::AbsPathBuf::new(std::env::temp_dir()).expect("temp cwd");
5275	    let opts = chat_session_spawn_options(
5276	        SessionInfo {
5277	            id: sid,
5278	            cwd: cwd.as_str().to_owned(),
5279	        },
5280	        cwd,
5281	        None,
5282	        None,
5283	        acp::ModelId::new(std::sync::Arc::from("test-model")),
5284	        false,
5285	    );
5286	    assert!(opts.mcp_servers.is_empty());
5287	    assert!(opts.initial_client_mcp_servers.is_empty());
5288	    assert!(!opts.client_code_nav_enabled);
5289	    assert!(!opts.client_terminal);
5290	    assert!(!opts.client_fs_read);
5291	    assert!(!opts.client_fs_write);
5292	    assert!(opts.chat_history.is_empty());
5293	    assert!(!opts.session_auto_mode);
5294	    assert!(
5295	        opts.persistence.is_noop(),
5296	        "K10 thin profile must use PersistenceHandle::noop()"
5297	    );
5298	    assert!(opts.is_chat_kind);
5299	}
5300	/// Drives the real `session/new` path so a later `load_session_inner` is a genuine attach.
5301	async fn new_root_session(agent: &MvpAgent, cwd: &std::path::Path) -> acp::SessionId {
5302	    agent.set_auth_method(acp::AuthMethodId::new("cached_token"));
5303	    let init = acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
5304	        acp::ClientCapabilities::new()
5305	            .fs(acp::FileSystemCapabilities::new())
5306	            .terminal(false),
5307	    );
5308	    agent.initialize_request.set(init).unwrap();
5309	    let sid = uuid::Uuid::now_v7().to_string();
5310	    let meta = serde_json::json!({ "sessionId": sid, "modelId": "test-model" })
5311	        .as_object()
5312	        .cloned();
5313	    agent
5314	        .new_session_inner(acp::NewSessionRequest::new(cwd.to_path_buf()).meta(meta))
5315	        .await
5316	        .expect("session/new succeeds")
5317	        .session_id
5318	}
5319	/// Bindings the real spawn path took through the agent's (lazily built) local workspace ops.
5320	fn workspace_session_count(agent: &MvpAgent) -> usize {
5321	    agent
5322	        .workspace_ops
5323	        .borrow()
5324	        .as_ref()
5325	        .and_then(xai_grok_workspace::WorkspaceOps::workspace_handle)
5326	        .map_or(
5327	            0,
5328	            xai_grok_workspace::handle::WorkspaceHandle::session_count,
5329	        )
5330	}
5331	/// The toolset the real spawn path bound for `sid`; its pointer identifies that binding.
5332	fn workspace_toolset(
5333	    agent: &MvpAgent,
5334	    sid: &acp::SessionId,
5335	) -> std::sync::Arc<xai_grok_tools::registry::types::FinalizedToolset> {
5336	    agent
5337	        .workspace_ops
5338	        .borrow()
5339	        .as_ref()
5340	        .and_then(xai_grok_workspace::WorkspaceOps::workspace_handle)
5341	        .and_then(|handle| handle.session(sid.0.as_ref()))
5342	        .expect("the installed actor bound its workspace session")
5343	        .toolset()
5344	}
5345	/// The mailbox is FIFO, so once the ack returns every earlier stamp has been served and counted.
5346	async fn drain_persistence(agent: &MvpAgent, sid: &acp::SessionId) {
5347	    let handle = agent.resident_handle(sid).expect("resident actor to drain");
5348	    let (respond_to, ack) = tokio::sync::oneshot::channel();
5349	    handle
5350	        .persistence_tx
5351	        .send(crate::session::persistence::PersistenceMsg::FlushAndAck { respond_to })
5352	        .expect("persistence actor is alive");
5353	    ack.await
5354	        .expect("persistence acks the flush")
5355	        .expect("flush succeeds");
5356	}
5357	#[test]
5358	fn new_session_registers_root_identity() {
5359	    run_local_for_bridge_test(|| async {
5360	        let agent = build_minimal_agent_for_tests();
5361	        let cwd = tempfile::tempdir().unwrap();
5362	        let sid = new_root_session(&agent, cwd.path()).await;
5363	        let root = agent
5364	            .session_registry
5365	            .root_for_session(&sid)
5366	            .expect("session/new registers root identity");
5367	        assert_root_views(&agent, &sid, Some(&root));
5368	        agent.remove_session(&sid);
5369	        assert_root_views(&agent, &sid, None);
5370	        crate::session::persistence::STAMPS_SERVED.set(0);
5371	        let request = || acp::LoadSessionRequest::new(sid.clone(), cwd.path().to_path_buf());
5372	        let (first, reached, release) = with_pause_at(
5373	            AttachPause::AfterInstall,
5374	            agent.load_session_inner(request()),
5375	        );
5376	        let overlap_generation = std::cell::Cell::new(None);
5377	        let overlap_attempt = std::cell::RefCell::new(None);
5378	        let second = async {
5379	            reached.await.expect("cold attach installed its actor");
5380	            agent.remove_session(&sid);
5381	            agent
5382	                .load_session_inner(request())
5383	                .await
5384	                .expect("overlapping cold attach settles");
5385	            let overlap_root = agent
5386	                .session_registry
5387	                .root_for_session(&sid)
5388	                .expect("winning warm attach publishes installed identity");
5389	            assert_eq!(root.agent_id, overlap_root.agent_id);
5390	            assert_ne!(root.generation, overlap_root.generation);
5391	            overlap_generation.set(Some(overlap_root.generation));
5392	            overlap_attempt.replace(Some(overlap_root.attempt_id.clone()));
5393	            assert_root_views(&agent, &sid, Some(&overlap_root));
5394	            let _ = release.send(());
5395	        };
5396	        let (first_result, ()) = tokio::join!(first, second);
5397	        let error = first_result.expect_err("superseded cold attach fails");
5398	        assert!(error.data.is_some());
5399	        assert_eq!(
5400	            1,
5401	            workspace_session_count(&agent),
5402	            "the superseded loader must leave its replacement's workspace binding alone"
5403	        );
5404	        drain_persistence(&agent, &sid).await;
5405	        assert_eq!(
5406	            1,
5407	            crate::session::persistence::STAMPS_SERVED.get(),
5408	            "winner stamps once, superseded loader stamps zero"
5409	        );
5410	        let summary_path = crate::util::grok_home::sessions_cwd_dir(&cwd.path().to_string_lossy())
5411	            .join(sid.0.as_ref())
5412	            .join("summary.json");
5413	        let summary: crate::session::persistence::Summary =
5414	            serde_json::from_slice(&std::fs::read(summary_path).expect("winning summary persists"))
5415	                .expect("winning summary parses");
5416	        assert_eq!(
5417	            overlap_attempt.borrow().as_ref().map(ToString::to_string),
5418	            summary.attempt_id,
5419	        );
5420	        let warm = agent
5421	            .load_session_inner(request())
5422	            .await
5423	            .expect("warm attach succeeds");
5424	        drop(warm);
5425	        let warm_root = agent
5426	            .session_registry
5427	            .root_for_session(&sid)
5428	            .expect("warm attach republishes resident identity");
5429	        assert_eq!(root.agent_id, warm_root.agent_id);
5430	        assert_eq!(overlap_generation.get(), Some(warm_root.generation));
5431	        assert_ne!(root.attempt_id, warm_root.attempt_id);
5432	        assert_root_views(&agent, &sid, Some(&warm_root));
5433	        drain_persistence(&agent, &sid).await;
5434	        assert_eq!(1, crate::session::persistence::STAMPS_SERVED.get());
5435	        agent.remove_session(&sid);
5436	    });
5437	}
5438	#[test]
5439	fn new_session_records_setup_phases_for_bisection() {
5440	    xai_grok_telemetry::unified_log::redirect_to_temp_for_tests();
5441	    run_local_for_bridge_test(|| async {
5442	        let agent = build_minimal_agent_for_tests();
5443	        let cwd = tempfile::tempdir().unwrap();
5444	        let mark = xai_grok_telemetry::unified_log::snapshot_log()
5445	            .unwrap_or_default()
5446	            .len();
5447	        let sid = new_root_session(&agent, cwd.path()).await;
5448	        agent.remove_session(&sid);
5449	        let log = xai_grok_telemetry::unified_log::snapshot_log().unwrap_or_default();
5450	        let appended = String::from_utf8_lossy(log.get(mark.min(log.len())..).unwrap_or(&[]));
5451	        for phase in [
5452	            "resolve_workspace",
5453	            "plugin_registry",
5454	            "mcp_merge",
5455	            "persistence_init",
5456	            "spawn_session_actor",
5457	            "git_discovery",
5458	        ] {
5459	            let needle = format!("\"phase\":\"{phase}\"");
5460	            assert!(
5461	                appended.contains(needle.as_str()),
5462	                "session/new must record {phase} in unified.jsonl; got:\n{appended}"
5463	            );
5464	        }
5465	        assert!(appended.contains("\"msg\":\"session created\""));
5466	    });
5467	}
5468	#[test]
5469	fn cold_load_stamps_identity_exactly_once() {
5470	    run_local_for_bridge_test(|| async {
5471	        let agent = build_minimal_agent_for_tests();
5472	        let cwd = tempfile::tempdir().unwrap();
5473	        let sid = new_root_session(&agent, cwd.path()).await;
5474	        agent.remove_session(&sid);
5475	        crate::session::persistence::STAMPS_SERVED.set(0);
5476	        agent
5477	            .load_session_inner(acp::LoadSessionRequest::new(
5478	                sid.clone(),
5479	                cwd.path().to_path_buf(),
5480	            ))
5481	            .await
5482	            .expect("cold attach succeeds");
5483	        drain_persistence(&agent, &sid).await;
5484	        assert_eq!(1, crate::session::persistence::STAMPS_SERVED.get());
5485	        agent.remove_session(&sid);
5486	    });
5487	}
5488	#[test]
5489	fn failed_identity_stamp_fails_the_cold_load_closed() {
5490	    run_local_for_bridge_test(|| async {
5491	        let agent = build_minimal_agent_for_tests();
5492	        let cwd = tempfile::tempdir().unwrap();
5493	        let sid = new_root_session(&agent, cwd.path()).await;
5494	        agent.remove_session(&sid);
5495	        assert_eq!(0, workspace_session_count(&agent));
5496	        crate::session::persistence::STAMPS_SERVED.set(0);
5497	        crate::session::persistence::FAIL_NEXT_STAMP.set(true);
5498	        let request = || acp::LoadSessionRequest::new(sid.clone(), cwd.path().to_path_buf());
5499	        let (load, reached, release) = with_pause_at(
5500	            AttachPause::AfterInstall,
5501	            agent.load_session_inner(request()),
5502	        );
5503	        let toolset = async {
5504	            reached.await.expect("cold attach installed its actor");
5505	            let toolset = std::sync::Arc::downgrade(&workspace_toolset(&agent, &sid));
5506	            let _ = release.send(());
5507	            toolset
5508	        };
5509	        let (result, toolset) = tokio::join!(load, toolset);
5510	        let error = result.expect_err("a stamp that did not land fails the attach");
5511	        assert_eq!(
5512	            Some(serde_json::json!("session load was superseded")),
5513	            error.data
5514	        );
5515	        assert!(!crate::session::persistence::FAIL_NEXT_STAMP.get());
5516	        assert_eq!(1, crate::session::persistence::STAMPS_SERVED.get());
5517	        assert!(
5518	            !agent.is_resident(&sid),
5519	            "failed attach must not leak a resident actor"
5520	        );
5521	        assert_eq!(0, agent.resident_count());
5522	        assert_root_views(&agent, &sid, None);
5523	        assert_eq!(
5524	            0,
5525	            workspace_session_count(&agent),
5526	            "the failed install must release its workspace binding"
5527	        );
5528	        assert!(
5529	            toolset.upgrade().is_none(),
5530	            "nothing may keep the failed install's toolset alive"
5531	        );
5532	    });
5533	}
5534	/// The binding follows the registry's decision: an install the registry refuses never touches the
5535	/// workspace map, whether the id was closed or superseded while its actor was spawning.
5536	#[test]
5537	fn stale_install_takes_no_workspace_binding() {
5538	    run_local_for_bridge_test(|| async {
5539	        let agent = build_minimal_agent_for_tests();
5540	        let cwd = tempfile::tempdir().unwrap();
5541	        let sid = new_root_session(&agent, cwd.path()).await;
5542	        agent.remove_session(&sid);
5543	        crate::session::persistence::STAMPS_SERVED.set(0);
5544	        let request = || acp::LoadSessionRequest::new(sid.clone(), cwd.path().to_path_buf());
5545	        let (stale, reached, release) =
5546	            with_pause_at(AttachPause::AfterSpawn, agent.load_session_inner(request()));
5547	        tokio::pin!(stale);
5548	        tokio::select! {
5549	            reached = reached => reached.expect("the install received its actor's init result"),
5550	            settled = &mut stale => panic!("the install settled without pausing: {settled:?}"),
5551	        }
5552	        assert_eq!(
5553	            0,
5554	            workspace_session_count(&agent),
5555	            "nothing binds before install"
5556	        );
5557	        agent.remove_session(&sid);
5558	        let _ = release.send(());
5559	        let error = stale.await.expect_err("a refused install fails closed");
5560	        assert_eq!(
5561	            Some(serde_json::json!("session load was superseded")),
5562	            error.data
5563	        );
5564	        assert_eq!(0, crate::session::persistence::STAMPS_SERVED.get());
5565	        assert!(!agent.is_resident(&sid));
5566	        assert_eq!(
5567	            0,
5568	            workspace_session_count(&agent),
5569	            "a refused install takes no binding"
5570	        );
5571	        let (stale, reached, release) =
5572	            with_pause_at(AttachPause::AfterSpawn, agent.load_session_inner(request()));
5573	        tokio::pin!(stale);
5574	        tokio::select! {
5575	            reached = reached => reached.expect("the install received its actor's init result"),
5576	            settled = &mut stale => panic!("the install settled without pausing: {settled:?}"),
5577	        }
5578	        let supersede = agent.begin_session_load(&sid);
5579	        let _ = release.send(());
5580	        let error = stale.await.expect_err("a refused install fails closed");
5581	        assert_eq!(
5582	            Some(serde_json::json!("session load was superseded")),
5583	            error.data
5584	        );
5585	        assert_eq!(0, workspace_session_count(&agent));
5586	        drop(supersede);
5587	        agent
5588	            .load_session_inner(request())
5589	            .await
5590	            .expect("the successor's cold attach succeeds");
5591	        assert_eq!(1, crate::session::persistence::STAMPS_SERVED.get());
5592	        assert_eq!(1, workspace_session_count(&agent));
5593	        assert!(agent.is_resident(&sid));
5594	        agent.remove_session(&sid);
5595	    });
5596	}
5597	/// The last-writer-wins ordering: the winner is resident and bound before the slower install's
5598	/// result is acted on. A bind from the loser's spawn would replace the winner's live binding and
5599	/// its rollback would then unmap it; the loser must never reach the workspace map at all.
5600	#[test]
5601	fn stale_install_never_binds_over_the_winners_workspace_session() {
5602	    run_local_for_bridge_test(|| async {
5603	        let agent = build_minimal_agent_for_tests();
5604	        let cwd = tempfile::tempdir().unwrap();
5605	        let sid = new_root_session(&agent, cwd.path()).await;
5606	        agent.remove_session(&sid);
5607	        crate::session::persistence::STAMPS_SERVED.set(0);
5608	        let request = || acp::LoadSessionRequest::new(sid.clone(), cwd.path().to_path_buf());
5609	        let (loser, reached, release) =
5610	            with_pause_at(AttachPause::AfterSpawn, agent.load_session_inner(request()));
5611	        tokio::pin!(loser);
5612	        tokio::select! {
5613	            reached = reached => reached.expect("the install received its actor's init result"),
5614	            settled = &mut loser => panic!("the install settled without pausing: {settled:?}"),
5615	        }
5616	        assert_eq!(
5617	            0,
5618	            workspace_session_count(&agent),
5619	            "the loser's spawn binds nothing"
5620	        );
5621	        agent
5622	            .load_session_inner(request())
5623	            .await
5624	            .expect("the winning cold attach succeeds");
5625	        assert!(agent.is_resident(&sid));
5626	        let winner_toolset = workspace_toolset(&agent, &sid);
5627	        assert_eq!(1, workspace_session_count(&agent));
5628	        let _ = release.send(());
5629	        let error = loser.await.expect_err("a refused install fails closed");
5630	        assert_eq!(
5631	            Some(serde_json::json!("session load was superseded")),
5632	            error.data
5633	        );
5634	        assert_eq!(1, crate::session::persistence::STAMPS_SERVED.get());
5635	        assert_eq!(
5636	            1,
5637	            workspace_session_count(&agent),
5638	            "the loser's rollback must leave the winner's binding in place"
5639	        );
5640	        assert!(
5641	            std::sync::Arc::ptr_eq(&winner_toolset, &workspace_toolset(&agent, &sid)),
5642	            "the winner's toolset is still the bound one; the loser's never was"
5643	        );
5644	        assert!(agent.is_resident(&sid));
5645	        agent.remove_session(&sid);
5646	    });
5647	}
5648	#[test]
5649	fn stamp_rollback_leaves_an_overlapping_attach_binding_intact() {
5650	    run_local_for_bridge_test(|| async {
5651	        let agent = build_minimal_agent_for_tests();
5652	        let cwd = tempfile::tempdir().unwrap();
5653	        let sid = new_root_session(&agent, cwd.path()).await;
5654	        agent.remove_session(&sid);
5655	        crate::session::persistence::STAMPS_SERVED.set(0);
5656	        crate::session::persistence::FAIL_NEXT_STAMP.set(true);
5657	        let request = || acp::LoadSessionRequest::new(sid.clone(), cwd.path().to_path_buf());
5658	        let (failing, reached, release) = with_pause_at(
5659	            AttachPause::BeforeRelease,
5660	            agent.load_session_inner(request()),
5661	        );
5662	        let successor = async {
5663	            reached.await.expect("rollback drained its actor");
5664	            agent
5665	                .load_session_inner(request())
5666	                .await
5667	                .expect("overlapping cold attach succeeds");
5668	            let toolset = workspace_toolset(&agent, &sid);
5669	            let _ = release.send(());
5670	            toolset
5671	        };
5672	        let (failing_result, successor_toolset) = tokio::join!(failing, successor);
5673	        assert_eq!(
5674	            Some(serde_json::json!("session load was superseded")),
5675	            failing_result.expect_err("failed stamp fails closed").data
5676	        );
5677	        assert_eq!(2, crate::session::persistence::STAMPS_SERVED.get());
5678	        assert_eq!(1, workspace_session_count(&agent));
5679	        assert!(
5680	            std::sync::Arc::ptr_eq(&successor_toolset, &workspace_toolset(&agent, &sid)),
5681	            "the rollback must not unbind its successor's workspace"
5682	        );
5683	        assert!(agent.session_registry.root_for_session(&sid).is_some());
5684	        agent.remove_session(&sid);
5685	    });
5686	}
5687	/// The inverse ordering for the stamp rollback: the id is re-bound over the install's toolset while
5688	/// its stamp is outstanding, so a capture of "whatever is bound now" would name the newer binding.
5689	/// No second `session/load` can bind here (one that starts before the install refuses it, one that
5690	/// starts after adopts it), so the re-bind goes through the workspace directly.
5691	#[test]
5692	fn stamp_rollback_leaves_a_binding_taken_during_the_stamp_intact() {
5693	    run_local_for_bridge_test(|| async {
5694	        let agent = build_minimal_agent_for_tests();
5695	        let cwd = tempfile::tempdir().unwrap();
5696	        let sid = new_root_session(&agent, cwd.path()).await;
5697	        agent.remove_session(&sid);
5698	        crate::session::persistence::STAMPS_SERVED.set(0);
5699	        crate::session::persistence::FAIL_NEXT_STAMP.set(true);
5700	        let request = || acp::LoadSessionRequest::new(sid.clone(), cwd.path().to_path_buf());
5701	        let (failing, reached, release) = with_pause_at(
5702	            AttachPause::AfterInstall,
5703	            agent.load_session_inner(request()),
5704	        );
5705	        let rebind = async {
5706	            reached.await.expect("cold attach installed its actor");
5707	            let installed_toolset = std::sync::Arc::downgrade(&workspace_toolset(&agent, &sid));
5708	            let newer_toolset = std::sync::Arc::new(
5709	                xai_grok_tools::registry::types::FinalizedToolset::empty_for_test(),
5710	            );
5711	            let rebound = agent
5712	                .workspace_ops
5713	                .borrow()
5714	                .as_ref()
5715	                .expect("the spawn built the local workspace ops")
5716	                .bind_local_session(
5717	                    sid.0.as_ref(),
5718	                    cwd.path().to_path_buf(),
5719	                    xai_hunk_tracker::HunkTrackerHandle::noop(),
5720	                    std::sync::Arc::clone(&newer_toolset),
5721	                    None,
5722	                )
5723	                .expect("re-binding the installed id succeeds");
5724	            assert!(rebound, "the re-bind replaces the installed toolset");
5725	            let _ = release.send(());
5726	            (installed_toolset, newer_toolset)
5727	        };
5728	        let (failing_result, (installed_toolset, newer_toolset)) = tokio::join!(failing, rebind);
5729	        assert_eq!(
5730	            Some(serde_json::json!("session load was superseded")),
5731	            failing_result.expect_err("failed stamp fails closed").data
5732	        );
5733	        assert_eq!(1, crate::session::persistence::STAMPS_SERVED.get());
5734	        assert!(!agent.is_resident(&sid));
5735	        assert_eq!(
5736	            1,
5737	            workspace_session_count(&agent),
5738	            "the rollback must not unbind a binding taken during its stamp"
5739	        );
5740	        assert!(
5741	            std::sync::Arc::ptr_eq(&newer_toolset, &workspace_toolset(&agent, &sid)),
5742	            "the newer binding stays in place"
5743	        );
5744	        assert!(
5745	            installed_toolset.upgrade().is_none(),
5746	            "nothing may keep the withdrawn install's toolset alive"
5747	        );
5748	    });
5749	}
5750	/// Nothing of a withdrawn install may survive: no resident, no directory row, no workspace binding.
5751	fn assert_withdrawn(agent: &MvpAgent, sid: &acp::SessionId) {
5752	    assert!(
5753	        !agent.is_resident(sid),
5754	        "no unstamped actor may stay resident"
5755	    );
5756	    assert_eq!(0, agent.resident_count());
5757	    assert_root_views(agent, sid, None);
5758	    assert_eq!(0, workspace_session_count(agent));
5759	}
5760	/// Drives an adopting attach that must park on the installed actor's stamp, then fails that stamp.
5761	/// `before_adopt` runs with the installer parked after its install, before the adopting load starts.
5762	async fn adopting_attach_fails_with_the_stamp(
5763	    before_adopt: impl AsyncFn(&MvpAgent, &acp::SessionId),
5764	) {
5765	    let agent = build_minimal_agent_for_tests();
5766	    let cwd = tempfile::tempdir().unwrap();
5767	    let sid = new_root_session(&agent, cwd.path()).await;
5768	    agent.remove_session(&sid);
5769	    crate::session::persistence::STAMPS_SERVED.set(0);
5770	    let request = || acp::LoadSessionRequest::new(sid.clone(), cwd.path().to_path_buf());
5771	    let (cold, reached, release) = with_pause_at(
5772	        AttachPause::AfterInstall,
5773	        agent.load_session_inner(request()),
5774	    );
5775	    let adopt = async {
5776	        reached.await.expect("cold attach installed its actor");
5777	        before_adopt(&agent, &sid).await;
5778	        let adopt = agent.load_session_inner(request());
5779	        tokio::pin!(adopt);
5780	        let parked = async {
5781	            while agent.session_registry.identity_stamp_waiters(&sid) == 0 {
5782	                tokio::task::yield_now().await;
5783	            }
5784	        };
5785	        tokio::select! {
5786	            () = parked => {}
5787	            settled = &mut adopt => panic!("warm attach settled ahead of the stamp: {settled:?}"),
5788	        }
5789	        crate::session::persistence::FAIL_NEXT_STAMP.set(true);
5790	        let _ = release.send(());
5791	        adopt
5792	            .await
5793	            .expect_err("the adopting attach fails with the stamp")
5794	    };
5795	    let (cold_result, adopt_error) = tokio::join!(cold, adopt);
5796	    assert_eq!(
5797	        Some(serde_json::json!("session load was superseded")),
5798	        cold_result.expect_err("cold attach fails closed").data
5799	    );
5800	    assert_eq!(
5801	        Some(serde_json::json!("session identity stamp failed")),
5802	        adopt_error.data
5803	    );
5804	    assert_eq!(1, crate::session::persistence::STAMPS_SERVED.get());
5805	    assert_withdrawn(&agent, &sid);
5806	    agent
5807	        .load_session_inner(request())
5808	        .await
5809	        .expect("a fresh cold load succeeds after the failure");
5810	    drain_persistence(&agent, &sid).await;
5811	    assert_eq!(2, crate::session::persistence::STAMPS_SERVED.get());
5812	    assert!(agent.session_registry.root_for_session(&sid).is_some());
5813	    agent.remove_session(&sid);
5814	}
5815	#[test]
5816	fn adopting_attach_waits_for_the_installed_actors_stamp() {
5817	    run_local_for_bridge_test(|| adopting_attach_fails_with_the_stamp(async |_, _| {}));
5818	}
5819	/// A load that supersedes the installer and fails validation before it could await the stamp.
5820	/// Its guard settles the record as `Resident` while the installed actor's stamp is outstanding.
5821	async fn supersede_with_invalid_load(agent: &MvpAgent, sid: &acp::SessionId) {
5822	    let relative = std::path::PathBuf::from("relative/cwd");
5823	    agent
5824	        .load_session_inner(acp::LoadSessionRequest::new(sid.clone(), relative))
5825	        .await
5826	        .expect_err("a relative cwd fails validation");
5827	    assert!(!agent.session_registry.is_attaching(sid));
5828	    assert!(
5829	        agent.is_resident(sid),
5830	        "the early-invalid load settles the installed actor as resident"
5831	    );
5832	    assert_root_views(agent, sid, None);
5833	}
5834	/// Installs the actor for `sid`, lets an early-invalid load settle it as `Resident` ahead of its
5835	/// stamp, then releases the installer with its stamp set to land or fail; returns the installer's result.
5836	async fn install_then_settle_ahead_of_the_stamp(
5837	    agent: &MvpAgent,
5838	    sid: &acp::SessionId,
5839	    cwd: &std::path::Path,
5840	    fail_stamp: bool,
5841	) -> Result<acp::LoadSessionResponse, acp::Error> {
5842	    let request = acp::LoadSessionRequest::new(sid.clone(), cwd.to_path_buf());
5843	    let (cold, reached, release) =
5844	        with_pause_at(AttachPause::AfterInstall, agent.load_session_inner(request));
5845	    let supersede = async {
5846	        reached.await.expect("cold attach installed its actor");
5847	        supersede_with_invalid_load(agent, sid).await;
5848	        crate::session::persistence::FAIL_NEXT_STAMP.set(fail_stamp);
5849	        let _ = release.send(());
5850	    };
5851	    let (cold_result, ()) = tokio::join!(cold, supersede);
5852	    cold_result
5853	}
5854	/// `Failed` against a record a superseding guard already settled: no guard is left, so the record
5855	/// withdraws itself the way a cold attach that produced nothing does, entry and all.
5856	#[test]
5857	fn stamp_failure_on_a_settled_resident_releases_the_entry() {
5858	    run_local_for_bridge_test(|| async {
5859	        let agent = build_minimal_agent_for_tests();
5860	        let cwd = tempfile::tempdir().unwrap();
5861	        let sid = new_root_session(&agent, cwd.path()).await;
5862	        agent.remove_session(&sid);
5863	        crate::session::persistence::STAMPS_SERVED.set(0);
5864	        let error = install_then_settle_ahead_of_the_stamp(&agent, &sid, cwd.path(), true)
5865	            .await
5866	            .expect_err("a failed stamp fails the installer closed");
5867	        assert_eq!(
5868	            Some(serde_json::json!("session load was superseded")),
5869	            error.data
5870	        );
5871	        assert_eq!(1, crate::session::persistence::STAMPS_SERVED.get());
5872	        assert_withdrawn(&agent, &sid);
5873	        assert_eq!(None, agent.session_registry.live(&sid));
5874	        let counts = agent.session_registry.counts();
5875	        assert_eq!(0, counts.retained_resources + counts.resident_resources);
5876	        assert_eq!(
5877	            counts.session_threads, counts.entries,
5878	            "the failed load's entry goes with it; only a tracked thread may keep one"
5879	        );
5880	        agent
5881	            .load_session_inner(acp::LoadSessionRequest::new(
5882	                sid.clone(),
5883	                cwd.path().to_path_buf(),
5884	            ))
5885	            .await
5886	            .expect("a fresh cold load succeeds after the failure");
5887	        drain_persistence(&agent, &sid).await;
5888	        assert_eq!(2, crate::session::persistence::STAMPS_SERVED.get());
5889	        assert!(agent.session_registry.root_for_session(&sid).is_some());
5890	        agent.remove_session(&sid);
5891	    });
5892	}
5893	#[test]
5894	fn stamp_landing_on_a_settled_resident_publishes_it_once() {
5895	    run_local_for_bridge_test(|| async {
5896	        let agent = build_minimal_agent_for_tests();
5897	        let cwd = tempfile::tempdir().unwrap();
5898	        let sid = new_root_session(&agent, cwd.path()).await;
5899	        let root = agent
5900	            .session_registry
5901	            .root_for_session(&sid)
5902	            .expect("session/new registers root identity");
5903	        agent.remove_session(&sid);
5904	        crate::session::persistence::STAMPS_SERVED.set(0);
5905	        install_then_settle_ahead_of_the_stamp(&agent, &sid, cwd.path(), false)
5906	            .await
5907	            .expect("the installer completes once its stamp lands");
5908	        drain_persistence(&agent, &sid).await;
5909	        assert_eq!(1, crate::session::persistence::STAMPS_SERVED.get());
5910	        let published = agent
5911	            .session_registry
5912	            .root_for_session(&sid)
5913	            .expect("the landed stamp publishes the settled resident");
5914	        assert_eq!(root.agent_id, published.agent_id);
5915	        assert_ne!(root.attempt_id, published.attempt_id);
5916	        assert_eq!(1, agent.session_registry.snapshot_live_roots().len());
5917	        assert_root_views(&agent, &sid, Some(&published));
5918	        agent
5919	            .load_session_inner(acp::LoadSessionRequest::new(
5920	                sid.clone(),
5921	                cwd.path().to_path_buf(),
5922	            ))
5923	            .await
5924	            .expect("a warm attach adopts the stamped resident");
5925	        drain_persistence(&agent, &sid).await;
5926	        assert_eq!(
5927	            1,
5928	            crate::session::persistence::STAMPS_SERVED.get(),
5929	            "adopting a stamped resident does not re-stamp"
5930	        );
5931	        assert_root_views(&agent, &sid, Some(&published));
5932	        assert_eq!(1, workspace_session_count(&agent));
5933	        agent.remove_session(&sid);
5934	    });
5935	}
5936	/// A load cancelled with its actor installed and its stamp unresolved must withdraw that actor
5937	/// itself: a `Pending` slot nobody resolves would make every later adopter wait it out.
5938	#[test]
5939	fn a_cancelled_installer_withdraws_its_unstamped_actor() {
5940	    run_local_for_bridge_test(|| async {
5941	        let agent = build_minimal_agent_for_tests();
5942	        let cwd = tempfile::tempdir().unwrap();
5943	        let sid = new_root_session(&agent, cwd.path()).await;
5944	        agent.remove_session(&sid);
5945	        crate::session::persistence::STAMPS_SERVED.set(0);
5946	        let request = || acp::LoadSessionRequest::new(sid.clone(), cwd.path().to_path_buf());
5947	        let (cold, reached, _release) = with_pause_at(
5948	            AttachPause::AfterInstall,
5949	            agent.load_session_inner(request()),
5950	        );
5951	        let mut cold = Box::pin(cold);
5952	        tokio::select! {
5953	            reached = reached => reached.expect("cold attach installed its actor"),
5954	            settled = &mut cold => panic!("cold attach settled ahead of its pause: {settled:?}"),
5955	        }
5956	        drop(cold);
5957	        assert_withdrawn(&agent, &sid);
5958	        assert_eq!(None, agent.session_registry.live(&sid));
5959	        tokio::time::timeout(
5960	            std::time::Duration::from_secs(15),
5961	            agent.load_session_inner(request()),
5962	        )
5963	        .await
5964	        .expect("the next load must not wait out a stamp nobody will resolve")
5965	        .expect("the next load cold-spawns anew");
5966	        drain_persistence(&agent, &sid).await;
5967	        assert_eq!(
5968	            1,
5969	            crate::session::persistence::STAMPS_SERVED.get(),
5970	            "the cancelled installer never stamped; the fresh load stamps once"
5971	        );
5972	        assert_eq!(1, agent.session_registry.snapshot_live_roots().len());
5973	        assert!(agent.session_registry.root_for_session(&sid).is_some());
5974	        agent.remove_session(&sid);
5975	    });
5976	}
5977	/// A cancelled installer re-parks its still-running actor thread on the entry; settlement then
5978	/// restores the displaced presence, which carries a running thread of its own. Neither thread may
5979	/// be dropped: a dropped `JoinHandle` detaches the thread from the sweep.
5980	#[test]
5981	fn a_cancelled_installers_thread_stays_tracked_beside_the_displaced_one() {
5982	    run_local_for_bridge_test(|| async {
5983	        let agent = build_minimal_agent_for_tests();
5984	        let sid = acp::SessionId::new("sess-cancelled-installer-threads");
5985	        let blocked_thread = |release_rx: std::sync::mpsc::Receiver<()>| {
5986	            crate::session::SessionThread::from_handle(std::thread::spawn(move || {
5987	                let _ = release_rx.recv();
5988	            }))
5989	        };
5990	        let (release_displaced, displaced_rx) = std::sync::mpsc::channel::<()>();
5991	        agent
5992	            .session_registry
5993	            .set_thread(&sid, blocked_thread(displaced_rx));
5994	        let (_attach_tx, waiter) = agent.session_registry.begin_attach(&sid);
5995	        let (handle, _cmd_tx, _cmd_rx) = make_live_session_handle(&sid, None);
5996	        let (release_installed, installed_rx) = std::sync::mpsc::channel::<()>();
5997	        let Ok((_, install_id)) = agent.session_registry.put_resident_for_attach(
5998	            &sid,
5999	            handle,
6000	            blocked_thread(installed_rx),
6001	            None,
6002	            &waiter,
6003	        ) else {
6004	            panic!("the install is accepted");
6005	        };
6006	        let StampResolution::Withdrawn(withdrawn) =
6007	            agent
6008	                .session_registry
6009	                .resolve_identity_stamp(&sid, install_id, IdentityStamp::Failed)
6010	        else {
6011	            panic!("the unstamped install is withdrawn");
6012	        };
6013	        agent.roll_back_install_sync(&sid, *withdrawn);
6014	        agent.session_registry.settle_attach(&sid, &waiter);
6015	        assert!(!agent.is_resident(&sid));
6016	        assert_eq!(
6017	            2,
6018	            agent.session_registry.counts().session_threads,
6019	            "the withdrawn thread must stay tracked beside the restored presence's own"
6020	        );
6021	        drop(release_displaced);
6022	        drop(release_installed);
6023	        for _ in 0..100 {
6024	            agent.sweep_dead_sessions();
6025	            if agent.session_registry.counts().session_threads == 0 {
6026	                break;
6027	            }
6028	            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
6029	        }
6030	        assert_eq!(
6031	            0,
6032	            agent.session_registry.counts().session_threads,
6033	            "the sweep must reap both threads once they exit"
6034	        );
6035	        assert_eq!(0, agent.session_registry.counts().entries);
6036	    });
6037	}
6038	/// The cancelled installer's guard re-parks its withdrawn actor's thread on the live `Attaching`
6039	/// slot. A load that began while the installer was still hosted then finds no handle, cold-spawns
6040	/// on the real path, and its install takes that slot: the displaced thread must be retired for the
6041	/// sweep, not dropped with its `JoinHandle`.
6042	#[test]
6043	fn a_cancelled_installer_retires_its_thread_when_a_reinstall_takes_the_slot() {
6044	    run_local_for_bridge_test(|| async {
6045	        let agent = build_minimal_agent_for_tests();
6046	        let cwd = tempfile::tempdir().unwrap();
6047	        let sid = new_root_session(&agent, cwd.path()).await;
6048	        assert_eq!(
6049	            crate::agent::mvp_agent::session_lifecycle::CloseOutcome::Closed,
6050	            agent.close_active_session(&sid).await
6051	        );
6052	        crate::session::persistence::STAMPS_SERVED.set(0);
6053	        let (_installer_attach, installer_waiter) = agent.session_registry.begin_attach(&sid);
6054	        let (handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
6055	        tokio::task::spawn_local(async move {
6056	            while let Some(cmd) = cmd_rx.recv().await {
6057	                match cmd {
6058	                    TestSessionCommand::FlushComplete { respond_to } => {
6059	                        let _ = respond_to.send(Ok(()));
6060	                    }
6061	                    TestSessionCommand::IsBusy { respond_to } => {
6062	                        let _ = respond_to.send(false);
6063	                    }
6064	                    _ => {}
6065	                }
6066	            }
6067	        });
6068	        let (release_installer, installer_rx) = std::sync::mpsc::channel::<()>();
6069	        let installer_thread =
6070	            crate::session::SessionThread::from_handle(std::thread::spawn(move || {
6071	                let _ = installer_rx.recv();
6072	            }));
6073	        let Ok((_, install_id)) = agent.session_registry.put_resident_for_attach(
6074	            &sid,
6075	            handle,
6076	            installer_thread,
6077	            None,
6078	            &installer_waiter,
6079	        ) else {
6080	            panic!("the install is accepted");
6081	        };
6082	        let reinstall = agent.load_session_inner(acp::LoadSessionRequest::new(
6083	            sid.clone(),
6084	            cwd.path().to_path_buf(),
6085	        ));
6086	        tokio::pin!(reinstall);
6087	        assert!(
6088	            futures::poll!(reinstall.as_mut()).is_pending(),
6089	            "the load parks before deciding cold against warm"
6090	        );
6091	        assert!(agent.session_registry.is_attaching(&sid));
6092	        let StampResolution::Withdrawn(withdrawn) =
6093	            agent
6094	                .session_registry
6095	                .resolve_identity_stamp(&sid, install_id, IdentityStamp::Failed)
6096	        else {
6097	            panic!("the unstamped install is withdrawn");
6098	        };
6099	        agent.roll_back_install_sync(&sid, *withdrawn);
6100	        assert!(!agent.is_resident(&sid));
6101	        assert!(
6102	            agent.session_registry.has_thread(&sid),
6103	            "the withdrawn thread is re-parked on the live attach"
6104	        );
6105	        reinstall
6106	            .await
6107	            .expect("the load finds no handle, cold-spawns and installs");
6108	        drain_persistence(&agent, &sid).await;
6109	        assert!(agent.is_resident(&sid));
6110	        assert_eq!(1, crate::session::persistence::STAMPS_SERVED.get());
6111	        assert_eq!(
6112	            2,
6113	            agent.session_registry.counts().session_threads,
6114	            "the reinstall's thread holds the slot and the withdrawn thread is retired, not dropped"
6115	        );
6116	        drop(release_installer);
6117	        for _ in 0..100 {
6118	            agent.sweep_dead_sessions();
6119	            if agent.session_registry.counts().session_threads == 1 {
6120	                break;
6121	            }
6122	            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
6123	        }
6124	        assert_eq!(
6125	            1,
6126	            agent.session_registry.counts().session_threads,
6127	            "the sweep reaps the retired thread once it exits; the resident's stays"
6128	        );
6129	        assert!(agent.is_resident(&sid));
6130	        agent.remove_session(&sid);
6131	    });
6132	}
6133	#[tokio::test(flavor = "current_thread")]
6134	async fn spawn_seeds_root_conversation_group_in_turn_config() {
6135	    let local = tokio::task::LocalSet::new();
6136	    local
6137	        .run_until(async {
6138	            let agent = build_minimal_agent_for_tests();
6139	            agent.set_auth_method(acp::AuthMethodId::new("cached_token"));
6140	            let temp_dir = tempfile::tempdir().expect("temp session cwd");
6141	            let cwd = xai_grok_paths::AbsPathBuf::new(temp_dir.path().to_path_buf())
6142	                .expect("absolute temp session cwd");
6143	            let session_id = acp::SessionId::new("root-conversation-group-session");
6144	            let session_info = SessionInfo {
6145	                id: session_id.clone(),
6146	                cwd: cwd.as_str().to_owned(),
6147	            };
6148	            let model_id = agent.models_manager.current_model_id();
6149	            let mut options =
6150	                chat_session_spawn_options(session_info, cwd, None, None, model_id, false);
6151	            options.is_chat_kind = false;
6152	            let init = acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
6153	                acp::ClientCapabilities::new()
6154	                    .fs(acp::FileSystemCapabilities::new())
6155	                    .terminal(false),
6156	            );
6157	            agent
6158	                .spawn_and_register_session(&init, options, None)
6159	                .await
6160	                .expect("root session spawns");
6161	            let handle = agent
6162	                .resident_handle(&session_id)
6163	                .expect("spawn registers root session");
6164	            let sampling_config = handle
6165	                .chat_state_handle
6166	                .get_sampling_config()
6167	                .await
6168	                .expect("spawned root has sampling config");
6169	            assert_eq!(
6170	                sampling_config.conversation_group_id,
6171	                Some(crate::sampling::derive_conversation_group_id(
6172	                    session_id.0.as_ref(),
6173	                )),
6174	            );
6175	            agent.remove_session(&session_id);
6176	        })
6177	        .await;
6178	}
6179	/// `remove_session` releases the workspace binding and drains the per-session side maps.
6180	/// Test agents default to `workspace_ops = None`, so no other test reaches the release.
6181	/// `bind_local_session` builds a LocalTerminalBackend that uses `spawn_local`, so this must run on a LocalSet.
6182	#[tokio::test(flavor = "current_thread")]
6183	async fn remove_session_releases_workspace_binding_and_side_maps() {
6184	    tokio::task::LocalSet::new()
6185	        .run_until(async {
6186	            let agent = build_minimal_agent_for_tests();
6187	            let sid = acp::SessionId::new("test-session-workspace-release");
6188	            let ops = xai_grok_workspace::WorkspaceOps::for_test();
6189	            let toolset = std::sync::Arc::new(
6190	                xai_grok_tools::registry::types::FinalizedToolset::empty_for_test(),
6191	            );
6192	            let toolset_weak = std::sync::Arc::downgrade(&toolset);
6193	            ops.bind_local_session(
6194	                sid.0.as_ref(),
6195	                std::env::temp_dir(),
6196	                xai_hunk_tracker::HunkTrackerHandle::noop(),
6197	                toolset,
6198	                None,
6199	            )
6200	            .expect("bind_local_session must succeed");
6201	            assert!(toolset_weak.upgrade().is_some());
6202	            *agent.workspace_ops.borrow_mut() = Some(ops);
6203	            agent
6204	                .session_registry
6205	                .set_unavailable_model(&sid, acp::ModelId::new(std::sync::Arc::from("gone-model")));
6206	            agent.set_turn_number(&sid, 3);
6207	            let (_permission_tx, permission_rx) = tokio::sync::mpsc::unbounded_channel::<
6208	                xai_grok_workspace::permission::PermissionEvent,
6209	            >();
6210	            agent
6211	                .session_registry
6212	                .set_permission_receiver(&sid, permission_rx);
6213	            let _ = agent.session_registry.live_orphan_heal_lock(&sid);
6214	            assert_eq!(agent.session_registry.counts().live_orphan_heal_locks, 1);
6215	            let root = agent
6216	                .session_registry
6217	                .register_root(sid.clone(), test_root_identity(1, 2));
6218	            assert_root_views(&agent, &sid, Some(&root));
6219	            agent.retire_root_session(&sid);
6220	            assert_root_views(&agent, &sid, None);
6221	            agent.remove_session(&sid);
6222	            assert_root_views(&agent, &sid, None);
6223	            assert_eq!(
6224	                agent.session_registry.counts().live_orphan_heal_locks,
6225	                0,
6226	                "remove_session must evict the live-orphan heal mutex"
6227	            );
6228	            assert!(
6229	                toolset_weak.upgrade().is_none(),
6230	                "the workspace binding must release the toolset"
6231	            );
6232	            assert!(agent.session_registry.unavailable_model(&sid).is_none());
6233	            assert_eq!(agent.session_registry.counts().resident_resources, 0);
6234	            assert_eq!(
6235	                agent.session_registry.counts().retained_resources,
6236	                0,
6237	                "retained per-session resources must be reclaimed on removal"
6238	            );
6239	        })
6240	        .await;
6241	}
6242	/// Without a bridge, `ext_method` falls through to the unchanged local dispatch (`rewind::handle`), which reports the missing session.
6243	/// That proves the routing hook is skipped in local mode.
6244	#[test]
6245	fn ext_method_rewind_uses_local_dispatch_without_bridge() {
6246	    use acp::Agent as _;
6247	    let _env = crate::env::EnvVarGuard::remove(crate::env::GROK_DISABLE_CUSTOM_BRIDGE_ENV);
6248	    run_local_for_bridge_test(|| async {
6249	        let agent = build_minimal_agent_for_tests();
6250	        let params = serde_json::json!({ "sessionId": "sess-local" });
6251	        let err = agent
6252	            .ext_method(acp::ExtRequest::new(
6253	                "x.ai/rewind/points",
6254	                std::sync::Arc::from(serde_json::value::to_raw_value(&params).unwrap()),
6255	            ))
6256	            .await
6257	            .expect_err("local rewind with no session must error");
6258	        assert_eq!(err.code, acp::Error::resource_not_found(None).code);
6259	    });
6260	}
6261	#[test]
6262	fn cancel_does_not_forward_to_bridge_in_local_mode() {
6263	    use crate::session::SessionCommand;
6264	    use acp::Agent as _;
6265	    run_local_for_bridge_test(|| async {
6266	        let agent = build_minimal_agent_for_tests();
6267	        let sid = acp::SessionId::new("sess-cancel-local");
6268	        let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
6269	        agent.insert_resident(&sid, handle);
6270	        agent
6271	            .cancel(acp::CancelNotification::new(sid.clone()))
6272	            .await
6273	            .expect("cancel must succeed");
6274	        let mut saw_local_cancel = false;
6275	        while let Ok(cmd) = cmd_rx.try_recv() {
6276	            if let SessionCommand::Cancel(..) = cmd {
6277	                saw_local_cancel = true;
6278	            }
6279	        }
6280	        assert!(
6281	            saw_local_cancel,
6282	            "local-mode cancel dispatches the local SessionCommand::Cancel with no bridge attached"
6283	        );
6284	    });
6285	}
6286	/// Regression (post-cancel slot hang, first bad release 0.2.101; see `dispatch_lock`).
6287	/// SDK e2e shape: `test_cancel_ends_in_flight_turn_and_frees_slot` (grok-agent-sdk).
6288	#[test]
6289	fn cancel_never_overtakes_in_flight_prompt_intake() {
6290	    use crate::session::SessionCommand;
6291	    use acp::Agent as _;
6292	    run_local_for_bridge_test(|| async {
6293	        let agent = build_minimal_agent_for_tests();
6294	        let sid = acp::SessionId::new("sess-cancel-intake-race");
6295	        let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
6296	        agent.insert_resident(&sid, handle);
6297	        let order: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>> =
6298	            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
6299	        let (intake_parked_tx, intake_parked_rx) = tokio::sync::oneshot::channel::<()>();
6300	        let driver_order = order.clone();
6301	        tokio::task::spawn_local(async move {
6302	            let mut intake_parked_tx = Some(intake_parked_tx);
6303	            while let Some(cmd) = cmd_rx.recv().await {
6304	                match cmd {
6305	                    SessionCommand::GetCurrentPromptMode { .. } => {
6306	                        if let Some(tx) = intake_parked_tx.take() {
6307	                            let _ = tx.send(());
6308	                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
6309	                        }
6310	                    }
6311	                    SessionCommand::Prompt { .. } => driver_order.borrow_mut().push("prompt"),
6312	                    SessionCommand::Cancel(..) => driver_order.borrow_mut().push("cancel"),
6313	                    _ => {}
6314	                }
6315	            }
6316	        });
6317	        let prompt_fut = agent.prompt(acp::PromptRequest::new(
6318	            sid.clone(),
6319	            vec![acp::ContentBlock::from("hi")],
6320	        ));
6321	        let cancel_fut = async {
6322	            intake_parked_rx
6323	                .await
6324	                .expect("prompt intake reaches the fake actor");
6325	            let _ = agent
6326	                .cancel(acp::CancelNotification::new(sid.clone()))
6327	                .await;
6328	        };
6329	        let _ = futures::join!(prompt_fut, cancel_fut);
6330	        assert_eq!(
6331	            order.borrow().as_slice(),
6332	            ["prompt", "cancel"],
6333	            "cancel must land on the actor mailbox after the prompt it targets"
6334	        );
6335	    });
6336	}
6337	#[test]
6338	fn prompt_routes_only_non_send_now_through_human_delivery_handle() {
6339	    use acp::Agent as _;
6340	    run_local_for_bridge_test(|| async {
6341	        for (send_now, expected_handle_sends) in [(false, 1), (true, 0)] {
6342	            let agent = build_minimal_agent_for_tests();
6343	            let sid = acp::SessionId::new(format!("human-route-{send_now}"));
6344	            let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
6345	            agent.insert_resident(&sid, handle);
6346	            tokio::task::spawn_local(async move {
6347	                while let Some(command) = cmd_rx.recv().await {
6348	                    match command {
6349	                        SessionCommand::GetCurrentPromptMode { responds_to } => {
6350	                            let _ = responds_to.send(Default::default());
6351	                        }
6352	                        SessionCommand::GetCurrentModel { responds_to } => {
6353	                            let _ = responds_to.send(crate::session::CurrentModel {
6354	                                id: "test-model".to_owned(),
6355	                                reasoning_effort: None,
6356	                            });
6357	                        }
6358	                        SessionCommand::Prompt {
6359	                            prompt_blocks,
6360	                            client_identifier,
6361	                            screen_mode,
6362	                            verbatim,
6363	                            json_schema,
6364	                            send_now: actual,
6365	                            tool_overrides_update,
6366	                            respond_to,
6367	                            ..
6368	                        } => {
6369	                            assert_eq!(actual, send_now);
6370	                            assert_eq!(prompt_blocks.len(), 2);
6371	                            let Some(image) = prompt_blocks.get(1) else {
6372	                                panic!("expected image content block: {prompt_blocks:?}");
6373	                            };
6374	                            assert!(matches!(image, acp::ContentBlock::Image(_)));
6375	                            assert_eq!(client_identifier.as_deref(), Some("client"));
6376	                            assert_eq!(screen_mode.as_deref(), Some("minimal"));
6377	                            assert!(verbatim);
6378	                            assert_eq!(json_schema, Some(serde_json::json!({"type": "object"})));
6379	                            assert!(tool_overrides_update.is_some());
6380	                            let _ = respond_to
6381	                                .send(
6382	                                    Ok(crate::session::commands::PromptTurnOk {
6383	                                        stop_reason: acp::StopReason::Cancelled,
6384	                                        total_tokens: 0,
6385	                                        turn_snapshot: None,
6386	                                        completion_kind: crate::session::commands::PromptCompletionKind::RemovedFromQueue,
6387	                                        structured_output: None,
6388	                                        usage: None,
6389	                                        tool_overrides: None,
6390	                                    }),
6391	                                );
6392	                        }
6393	                        _ => {}
6394	                    }
6395	                }
6396	            });
6397	            let _ = crate::session::message_delivery::take_human_send_count();
6398	            let request = acp::PromptRequest::new(
6399	                sid,
6400	                vec![
6401	                    acp::ContentBlock::Text(acp::TextContent::new("hello")),
6402	                    acp::ContentBlock::Image(acp::ImageContent::new("data", "image/png")),
6403	                ],
6404	            )
6405	            .meta(
6406	                serde_json::json!({
6407	                    "sendNow": send_now,
6408	                    "clientIdentifier": "client",
6409	                    "screenMode": "minimal",
6410	                    "verbatim": true,
6411	                    "outputSchema": {"type": "object"},
6412	                    "toolOverrides": {"webSearch": {}},
6413	                })
6414	                .as_object()
6415	                .cloned(),
6416	            );
6417	            assert!(agent.prompt(request).await.is_ok());
6418	            assert_eq!(
6419	                crate::session::message_delivery::take_human_send_count(),
6420	                expected_handle_sends
6421	            );
6422	        }
6423	    });
6424	}
6425	use crate::session::SessionCommand as TestSessionCommand;
6426	/// Build a session handle wired to a *live* command channel.
6427	/// Returns the handle (move into `sessions`) plus a probe `cmd_tx`/`cmd_rx`.
6428	/// A test can observe what the agent sends to the actor and prove the channel is live.
6429	fn make_live_session_handle(
6430	    sid: &acp::SessionId,
6431	    running_prompt: Option<&str>,
6432	) -> (
6433	    crate::session::SessionHandle,
6434	    tokio::sync::mpsc::UnboundedSender<TestSessionCommand>,
6435	    tokio::sync::mpsc::UnboundedReceiver<TestSessionCommand>,
6436	) {
6437	    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
6438	    let mut handle = make_test_handle("test-model", false, Some("grok-tui"));
6439	    handle.cmd_tx = cmd_tx.clone();
6440	    handle.info = crate::session::info::Info {
6441	        id: sid.clone(),
6442	        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
6443	    };
6444	    if let Some(pid) = running_prompt {
6445	        *handle.current_prompt_id.lock().unwrap() = Some(pid.to_string());
6446	    }
6447	    (handle, cmd_tx, cmd_rx)
6448	}
6449	/// Spawn a minimal fake session actor on the `LocalSet` that answers `SessionCommand::IsBusy` with `busy`.
6450	/// Every other command is forwarded to the returned receiver so a test can assert on them (e.g. `Shutdown`).
6451	fn spawn_fake_actor(
6452	    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<TestSessionCommand>,
6453	    busy: bool,
6454	) -> tokio::sync::mpsc::UnboundedReceiver<TestSessionCommand> {
6455	    let (observed_tx, observed_rx) = tokio::sync::mpsc::unbounded_channel();
6456	    tokio::task::spawn_local(async move {
6457	        while let Some(cmd) = cmd_rx.recv().await {
6458	            match cmd {
6459	                TestSessionCommand::IsBusy { respond_to } => {
6460	                    let _ = respond_to.send(busy);
6461	                }
6462	                TestSessionCommand::PersistResumeStatus { respond_to } => {
6463	                    let _ = respond_to.send(());
6464	                }
6465	                other => {
6466	                    let _ = observed_tx.send(other);
6467	                }
6468	            }
6469	        }
6470	    });
6471	    observed_rx
6472	}
6473	/// Spawn a session actor that answers `IsBusy` from a live `active_work` counter —
6474	/// the same check `SessionActor::is_busy` performs — so a real `WorkGuard` drives it.
6475	fn spawn_active_work_actor(
6476	    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<TestSessionCommand>,
6477	    active_work: std::sync::Arc<std::sync::atomic::AtomicUsize>,
6478	) {
6479	    tokio::task::spawn_local(async move {
6480	        while let Some(cmd) = cmd_rx.recv().await {
6481	            if let TestSessionCommand::IsBusy { respond_to } = cmd {
6482	                let _ = respond_to.send(active_work.load(std::sync::atomic::Ordering::Acquire) > 0);
6483	            }
6484	        }
6485	    });
6486	}
6487	/// Drive `x.ai/internal/evict_sessions` through the real `ext_notification` handler path (not the internal helper).
6488	/// This matches how the leader server signals a client disconnect.
6489	async fn drive_disconnect(agent: &MvpAgent, sid: &acp::SessionId) {
6490	    drive_disconnect_many(agent, &[sid]).await;
6491	}
6492	/// Like `drive_disconnect`, but evicts several sessions in a single `x.ai/internal/evict_sessions` notification.
6493	/// That is the realistic shape of a real client disconnect.
6494	/// It is also the path that exercises `handle_evict_sessions`' concurrent `join_all` check pass followed by the sequential act pass.
6495	async fn drive_disconnect_many(agent: &MvpAgent, sids: &[&acp::SessionId]) {
6496	    use acp::Agent as _;
6497	    let ids: Vec<&str> = sids.iter().map(|s| s.0.as_ref()).collect();
6498	    let params = serde_json::json!({ "sessionIds": ids });
6499	    let params_json = serde_json::value::to_raw_value(&params).unwrap();
6500	    agent
6501	        .ext_notification(acp::ExtNotification::new(
6502	            "x.ai/internal/evict_sessions",
6503	            params_json.into(),
6504	        ))
6505	        .await
6506	        .expect("evict_sessions notification must be handled");
6507	}
6508	/// Drive `x.ai/session/close` through the real `ext_method` dispatch (`ext_method`, then `handlers::session::handle`, then `handle_session_close`).
6509	/// This exercises the exact production path that finalizes the replica.
6510	async fn drive_close(agent: &MvpAgent, session_id: &str) -> Result<acp::ExtResponse, acp::Error> {
6511	    use acp::Agent as _;
6512	    let params = serde_json::json!({ "sessionId": session_id });
6513	    let params_json = serde_json::value::to_raw_value(&params).unwrap();
6514	    agent
6515	        .ext_method(acp::ExtRequest::new(
6516	            "x.ai/session/close",
6517	            std::sync::Arc::from(params_json),
6518	        ))
6519	        .await
6520	}
6521	/// Every method `parse_queue_edit_command` accepts must be forwarded from `ext_notification` to that session's mailbox.
6522	/// Parser-only coverage misses a dispatch drop.
6523	#[tokio::test(flavor = "current_thread")]
6524	async fn ext_notification_forwards_each_queue_method_to_session_actor() {
6525	    use acp::Agent as _;
6526	    let agent = build_minimal_agent_for_tests();
6527	    let sid = acp::SessionId::new("sess-queue-rt");
6528	    let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
6529	    agent.insert_resident(&sid, handle);
6530	    let session_id = sid.0.as_ref();
6531	    let cases: [(&str, serde_json::Value); 7] = [
6532	        (
6533	            "x.ai/queue/remove",
6534	            serde_json::json!({
6535	                "sessionId": session_id,
6536	                "id": "p-remove",
6537	                "expectedVersion": 3,
6538	                "owner": "grok-tui",
6539	            }),
6540	        ),
6541	        (
6542	            "x.ai/queue/reorder",
6543	            serde_json::json!({
6544	                "sessionId": session_id,
6545	                "orderedIds": ["a", "b"],
6546	            }),
6547	        ),
6548	        (
6549	            "x.ai/queue/clear",
6550	            serde_json::json!({
6551	                "sessionId": session_id,
6552	                "clientIdentifier": "grok-desktop",
6553	            }),
6554	        ),
6555	        (
6556	            "x.ai/queue/edit",
6557	            serde_json::json!({
6558	                "sessionId": session_id,
6559	                "id": "p-edit",
6560	                "newText": "rewritten",
6561	                "owner": "grok-vscode",
6562	            }),
6563	        ),
6564	        (
6565	            "x.ai/queue/interject",
6566	            serde_json::json!({
6567	                "sessionId": session_id,
6568	                "id": "p-interject",
6569	                "expectedVersion": 2,
6570	                "owner": "grok-tui",
6571	                "newText": "now",
6572	            }),
6573	        ),
6574	        (
6575	            "x.ai/queue/hold_edit",
6576	            serde_json::json!({
6577	                "sessionId": session_id,
6578	                "id": "p-hold",
6579	            }),
6580	        ),
6581	        (
6582	            "x.ai/queue/release_edit",
6583	            serde_json::json!({
6584	                "sessionId": session_id,
6585	                "id": "p-release",
6586	            }),
6587	        ),
6588	    ];
6589	    for (method, params) in cases {
6590	        let params_json = serde_json::value::to_raw_value(&params).expect("serialize queue params");
6591	        agent
6592	            .ext_notification(acp::ExtNotification::new(method, params_json.into()))
6593	            .await
6594	            .unwrap_or_else(|e| panic!("{method} ext_notification failed: {e}"));
6595	        let cmd = cmd_rx.try_recv().unwrap_or_else(|e| {
6596	            panic!("{method} must land a SessionCommand on the actor mailbox, try_recv={e}")
6597	        });
6598	        match (method, cmd) {
6599	            (
6600	                "x.ai/queue/remove",
6601	                SessionCommand::RemoveQueuedPrompt {
6602	                    id,
6603	                    expected_version,
6604	                    owner,
6605	                },
6606	            ) => {
6607	                assert_eq!(id, "p-remove");
6608	                assert_eq!(expected_version, 3);
6609	                assert_eq!(owner.as_deref(), Some("grok-tui"));
6610	            }
6611	            ("x.ai/queue/reorder", SessionCommand::ReorderQueue { ordered_ids }) => {
6612	                assert_eq!(ordered_ids, vec!["a", "b"]);
6613	            }
6614	            ("x.ai/queue/clear", SessionCommand::ClearQueue { owner }) => {
6615	                assert_eq!(owner.as_deref(), Some("grok-desktop"));
6616	            }
6617	            (
6618	                "x.ai/queue/edit",
6619	                SessionCommand::EditQueuedPrompt {
6620	                    id,
6621	                    new_text,
6622	                    editor,
6623	                },
6624	            ) => {
6625	                assert_eq!(id, "p-edit");
6626	                assert_eq!(new_text, "rewritten");
6627	                assert_eq!(editor.as_deref(), Some("grok-vscode"));
6628	            }
6629	            (
6630	                "x.ai/queue/interject",
6631	                SessionCommand::InterjectQueuedPrompt {
6632	                    id,
6633	                    expected_version,
6634	                    owner,
6635	                    new_text,
6636	                },
6637	            ) => {
6638	                assert_eq!(id, "p-interject");
6639	                assert_eq!(expected_version, 2);
6640	                assert_eq!(owner.as_deref(), Some("grok-tui"));
6641	                assert_eq!(new_text.as_deref(), Some("now"));
6642	            }
6643	            ("x.ai/queue/hold_edit", SessionCommand::HoldEdit { id }) => {
6644	                assert_eq!(id, "p-hold");
6645	            }
6646	            ("x.ai/queue/release_edit", SessionCommand::ReleaseEdit { id }) => {
6647	                assert_eq!(id, "p-release");
6648	            }
6649	            (method, _) => {
6650	                panic!("{method} dispatched a SessionCommand of the wrong variant")
6651	            }
6652	        }
6653	    }
6654	    assert!(
6655	        matches!(
6656	            cmd_rx.try_recv(),
6657	            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
6658	        ),
6659	        "no extra SessionCommand may remain after the seven queue methods"
6660	    );
6661	}
6662	/// Methods the parser rejects (unknown, outbound `changed`, missing id / newText) and a missing session must not send a command or panic.
6663	#[tokio::test(flavor = "current_thread")]
6664	async fn ext_notification_queue_rejects_unknown_method_missing_id_and_unknown_session() {
6665	    use acp::Agent as _;
6666	    let agent = build_minimal_agent_for_tests();
6667	    let sid = acp::SessionId::new("sess-queue-neg");
6668	    let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
6669	    agent.insert_resident(&sid, handle);
6670	    let session_id = sid.0.as_ref();
6671	    let negatives: [(&str, serde_json::Value); 9] = [
6672	        (
6673	            "x.ai/queue/bogus",
6674	            serde_json::json!({ "sessionId": session_id, "id": "p1" }),
6675	        ),
6676	        (
6677	            "x.ai/queue/changed",
6678	            serde_json::json!({
6679	                "sessionId": session_id,
6680	                "entries": [{
6681	                    "id": "p1",
6682	                    "version": 0,
6683	                    "kind": "prompt",
6684	                    "text": "hello",
6685	                    "position": 0,
6686	                }],
6687	            }),
6688	        ),
6689	        (
6690	            "x.ai/queue/hold_edit",
6691	            serde_json::json!({ "sessionId": session_id }),
6692	        ),
6693	        (
6694	            "x.ai/queue/release_edit",
6695	            serde_json::json!({ "sessionId": session_id }),
6696	        ),
6697	        (
6698	            "x.ai/queue/remove",
6699	            serde_json::json!({ "sessionId": session_id }),
6700	        ),
6701	        (
6702	            "x.ai/queue/edit",
6703	            serde_json::json!({ "sessionId": session_id, "newText": "x" }),
6704	        ),
6705	        (
6706	            "x.ai/queue/edit",
6707	            serde_json::json!({ "sessionId": session_id, "id": "p-edit" }),
6708	        ),
6709	        (
6710	            "x.ai/queue/interject",
6711	            serde_json::json!({ "sessionId": session_id }),
6712	        ),
6713	        (
6714	            "x.ai/queue/hold_edit",
6715	            serde_json::json!({ "sessionId": "no-such-session", "id": "p1" }),
6716	        ),
6717	    ];
6718	    for (method, params) in negatives {
6719	        let params_json = serde_json::value::to_raw_value(&params).expect("serialize queue params");
6720	        agent
6721	            .ext_notification(acp::ExtNotification::new(method, params_json.into()))
6722	            .await
6723	            .unwrap_or_else(|e| panic!("{method} ext_notification must not fail: {e}"));
6724	        assert!(
6725	            matches!(
6726	                cmd_rx.try_recv(),
6727	                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
6728	            ),
6729	            "{method} must not send a SessionCommand (params={params})"
6730	        );
6731	    }
6732	    let agent_empty = build_minimal_agent_for_tests();
6733	    let params_json = serde_json::value::to_raw_value(&serde_json::json!({
6734	        "sessionId": "ghost",
6735	        "id": "p1",
6736	    }))
6737	    .expect("serialize");
6738	    agent_empty
6739	        .ext_notification(acp::ExtNotification::new(
6740	            "x.ai/queue/release_edit",
6741	            params_json.into(),
6742	        ))
6743	        .await
6744	        .expect("queue edit for a missing session must not error");
6745	}
6746	/// Fire-and-forget: a gone session actor must not turn `ext_notification`
6747	/// into an error or panic.
6748	#[tokio::test(flavor = "current_thread")]
6749	async fn ext_notification_queue_edit_survives_dropped_actor_mailbox() {
6750	    use acp::Agent as _;
6751	    let agent = build_minimal_agent_for_tests();
6752	    let sid = acp::SessionId::new("sess-queue-dead");
6753	    let (handle, _tx, cmd_rx) = make_live_session_handle(&sid, None);
6754	    agent.insert_resident(&sid, handle);
6755	    drop(cmd_rx);
6756	    let params = serde_json::json!({
6757	        "sessionId": sid.0.as_ref(),
6758	        "id": "p-hold",
6759	    });
6760	    let params_json = serde_json::value::to_raw_value(&params).expect("serialize queue params");
6761	    agent
6762	        .ext_notification(acp::ExtNotification::new(
6763	            "x.ai/queue/hold_edit",
6764	            params_json.into(),
6765	        ))
6766	        .await
6767	        .expect("queue edit must not error when the session actor mailbox is gone");
6768	}
6769	/// No-evict keystone: a client disconnecting mid-turn must NOT destroy the session. The actor stays resident and no `Shutdown` is sent.
6770	/// The resident session's command channel still **delivers** commands, so a reconnecting `session/load` can keep driving the turn. `finalize()` is NOT called on a mere disconnect.
6771	#[test]
6772	fn disconnect_keeps_live_session_resident_without_finalize() {
6773	    run_local_for_bridge_test(|| async {
6774	        let agent = build_minimal_agent_for_tests();
6775	        let sid = acp::SessionId::new("sess-live");
6776	        let (_cmd_tx, mut cmd_rx) = {
6777	            let (handle, tx, rx) = make_live_session_handle(&sid, Some("turn-1"));
6778	            agent.insert_resident(&sid, handle);
6779	            (tx, rx)
6780	        };
6781	        drive_disconnect(&agent, &sid).await;
6782	        assert!(
6783	            agent.is_resident(&sid),
6784	            "live session must stay resident across client disconnect"
6785	        );
6786	        assert!(
6787	            matches!(
6788	                cmd_rx.try_recv(),
6789	                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
6790	            ),
6791	            "no command may be sent to a session kept resident with live work"
6792	        );
6793	        let resident = agent
6794	            .resident_handle(&sid)
6795	            .expect("session must still be resident");
6796	        resident
6797	            .cmd_tx
6798	            .send(TestSessionCommand::ResetPermissionState)
6799	            .expect("resident session channel must accept commands post-disconnect");
6800	        assert!(
6801	            matches!(
6802	                cmd_rx.try_recv(),
6803	                Ok(TestSessionCommand::ResetPermissionState)
6804	            ),
6805	            "the resident session's receiver must observe the delivered command"
6806	        );
6807	        assert!(
6808	            agent.finalize_spy.borrow().is_empty(),
6809	            "finalize() must NOT fire on client disconnect"
6810	        );
6811	        assert_eq!(
6812	            agent.session_live_state_for(&sid),
6813	            Some(SessionLiveState::Working),
6814	            "a kept-resident session with live work is Working"
6815	        );
6816	    });
6817	}
6818	/// Keep-resident must hold even if the `current_prompt_id` lock is poisoned: an unknown state is treated as "busy" (never unload).
6819	/// Guards against a regression flipping the `unwrap_or(true)` fallback to `false`.
6820	#[test]
6821	fn disconnect_keeps_resident_on_poisoned_lock() {
6822	    run_local_for_bridge_test(|| async {
6823	        let agent = build_minimal_agent_for_tests();
6824	        let sid = acp::SessionId::new("sess-poison");
6825	        let (handle, _tx, _rx) = make_live_session_handle(&sid, None);
6826	        let poison_target = handle.current_prompt_id.clone();
6827	        agent.insert_resident(&sid, handle);
6828	        let _ = std::thread::spawn(move || {
6829	            let _g = poison_target.lock().unwrap();
6830	            panic!("poison current_prompt_id");
6831	        })
6832	        .join();
6833	        assert!(
6834	            agent
6835	                .resident_handle(&sid)
6836	                .unwrap()
6837	                .current_prompt_id
6838	                .lock()
6839	                .is_err(),
6840	            "precondition: the lock must be poisoned"
6841	        );
6842	        drive_disconnect(&agent, &sid).await;
6843	        assert!(
6844	            agent.is_resident(&sid),
6845	            "a session with an unknown (poisoned) state must be kept resident"
6846	        );
6847	        assert_eq!(
6848	            agent.session_live_state_for(&sid),
6849	            Some(SessionLiveState::Working),
6850	        );
6851	    });
6852	}
6853	/// A wedged actor stays tracked.
6854	/// `remove_session` releases everything else but keeps a still-running thread.
6855	/// Dropping its handle would detach the thread and leave nothing for the supervisor sweep to find.
6856	#[test]
6857	fn remove_session_keeps_a_running_thread_tracked() {
6858	    run_local_for_bridge_test(|| async {
6859	        let agent = build_minimal_agent_for_tests();
6860	        let sid = acp::SessionId::new("sess-wedged");
6861	        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
6862	        agent.session_registry.set_thread(
6863	            &sid,
6864	            crate::session::SessionThread::from_handle(std::thread::spawn(move || {
6865	                let _ = release_rx.recv();
6866	            })),
6867	        );
6868	        agent.set_turn_number(&sid, 1);
6869	        agent.remove_session(&sid);
6870	        assert!(
6871	            agent.session_registry.has_thread(&sid),
6872	            "a running actor thread must survive removal for the sweep"
6873	        );
6874	        assert_eq!(
6875	            agent.session_registry.counts().retained_resources,
6876	            0,
6877	            "everything except the running thread must be released"
6878	        );
6879	        drop(release_tx);
6880	        for _ in 0..100 {
6881	            agent.sweep_dead_sessions();
6882	            if !agent.session_registry.has_thread(&sid) {
6883	                break;
6884	            }
6885	            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
6886	        }
6887	        assert!(
6888	            !agent.session_registry.has_thread(&sid),
6889	            "the sweep must reclaim the thread once it exits"
6890	        );
6891	    });
6892	}
6893	/// Idle-unload stub (memory bound) and its supervisor interaction. A *fully idle* session is unloaded to disk on disconnect (actor `Shutdown`, handle dropped).
6894	/// The `SessionThread` is **retained** for `drain_old_session_thread`. It is not finalized, and once the kept thread finishes the supervisor reaps it as a *clean* exit, never `DeadFailed`.
6895	#[test]
6896	fn disconnect_unloads_idle_session_without_finalize() {
6897	    run_local_for_bridge_test(|| async {
6898	        let agent = build_minimal_agent_for_tests();
6899	        let sid = acp::SessionId::new("sess-idle");
6900	        let (handle, _cmd_tx, cmd_rx) = make_live_session_handle(&sid, None);
6901	        agent.insert_resident(&sid, handle);
6902	        let mut observed = spawn_fake_actor(cmd_rx, false);
6903	        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
6904	        agent.session_registry.set_thread(
6905	            &sid,
6906	            crate::session::SessionThread::from_handle(std::thread::spawn(move || {
6907	                let _ = release_rx.recv();
6908	            })),
6909	        );
6910	        agent.ensure_session_supervisor();
6911	        drive_disconnect(&agent, &sid).await;
6912	        assert!(
6913	            !agent.is_resident(&sid),
6914	            "idle session must be unloaded from the resident map on disconnect"
6915	        );
6916	        assert!(
6917	            agent.session_registry.has_thread(&sid),
6918	            "idle-unload must keep the SessionThread for reconnect drain"
6919	        );
6920	        let shutdown = tokio::time::timeout(std::time::Duration::from_secs(1), observed.recv())
6921	            .await
6922	            .expect("idle-unload must send a command within 1s")
6923	            .expect("fake actor channel must stay open");
6924	        assert!(
6925	            matches!(shutdown, TestSessionCommand::Shutdown(_)),
6926	            "idle-unload must send SessionCommand::Shutdown"
6927	        );
6928	        assert!(
6929	            agent.finalize_spy.borrow().is_empty(),
6930	            "idle-unload on disconnect must NOT finalize the cloud replica"
6931	        );
6932	        assert_eq!(
6933	            agent.session_live_state_for(&sid),
6934	            Some(SessionLiveState::Dormant),
6935	            "an idle-unloaded session demotes to Dormant"
6936	        );
6937	        drop(release_tx);
6938	        let deadline = tokio::time::Instant::now() + (SESSION_SUPERVISOR_TICK * 6);
6939	        while tokio::time::Instant::now() < deadline {
6940	            if !agent.session_registry.has_thread(&sid) {
6941	                break;
6942	            }
6943	            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
6944	        }
6945	        assert!(
6946	            !agent.session_registry.has_thread(&sid),
6947	            "supervisor must drop the finished kept thread"
6948	        );
6949	        assert!(
6950	            !agent
6951	                .roster_delta_spy
6952	                .borrow()
6953	                .iter()
6954	                .any(|(id, st)| id == sid.0.as_ref() && *st == SessionLiveState::DeadFailed),
6955	            "a cleanly idle-unloaded session must not be reaped as DeadFailed"
6956	        );
6957	        assert_eq!(
6958	            agent.session_live_state_for(&sid),
6959	            None,
6960	            "clean-exit sweep must drop the Dormant live-state entry"
6961	        );
6962	    });
6963	}
6964	#[test]
6965	fn evict_sessions_refuses_the_next_prompt() {
6966	    use acp::Agent as _;
6967	    run_local_for_bridge_test(|| async {
6968	        let agent = build_minimal_agent_for_tests();
6969	        let sid = acp::SessionId::new("sess-evict-prompt");
6970	        let (handle, _cmd_tx, cmd_rx) = make_live_session_handle(&sid, None);
6971	        agent.insert_resident(&sid, handle);
6972	        drop(spawn_fake_actor(cmd_rx, false));
6973	        drive_disconnect(&agent, &sid).await;
6974	        let err = agent
6975	            .prompt(acp::PromptRequest::new(
6976	                sid,
6977	                vec![acp::ContentBlock::from("next")],
6978	            ))
6979	            .await
6980	            .expect_err("an evicted session must refuse the next prompt");
6981	        assert_eq!(acp::Error::invalid_params().code, err.code);
6982	        assert_eq!(
6983	            Some("unknown session id"),
6984	            err.data.as_ref().and_then(|data| data.as_str())
6985	        );
6986	    });
6987	}
6988	/// The `IsBusy` keep-resident path. A between-turns session (`current_prompt_id = None`) whose actor answers `IsBusy = true` must be kept resident. True here means inputs are queued at the turn boundary.
6989	/// It must NOT be unloaded and must receive no `Shutdown`. This exercises the async round-trip that the sync fast-path tests skip.
6990	#[test]
6991	fn disconnect_keeps_resident_when_actor_reports_busy() {
6992	    run_local_for_bridge_test(|| async {
6993	        let agent = build_minimal_agent_for_tests();
6994	        let sid = acp::SessionId::new("sess-busy");
6995	        let (handle, _cmd_tx, cmd_rx) = make_live_session_handle(&sid, None);
6996	        agent.insert_resident(&sid, handle);
6997	        let mut observed = spawn_fake_actor(cmd_rx, true);
6998	        drive_disconnect(&agent, &sid).await;
6999	        assert!(
7000	            agent.is_resident(&sid),
7001	            "a between-turns session with queued work (IsBusy=true) must stay resident"
7002	        );
7003	        assert_eq!(
7004	            agent.session_live_state_for(&sid),
7005	            Some(SessionLiveState::Working),
7006	            "an actor-reported-busy session is kept Working"
7007	        );
7008	        tokio::task::yield_now().await;
7009	        assert!(
7010	            matches!(
7011	                observed.try_recv(),
7012	                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
7013	            ),
7014	            "a busy session must not be sent Shutdown"
7015	        );
7016	    });
7017	}
7018	#[test]
7019	fn disconnect_keeps_resident_when_plan_approval_parked() {
7020	    run_local_for_bridge_test(|| async {
7021	        let agent = build_minimal_agent_for_tests();
7022	        let sid = acp::SessionId::new("sess-plan-parked");
7023	        let (handle, _cmd_tx, cmd_rx) = make_live_session_handle(&sid, None);
7024	        handle.pending_interactions.lock().unwrap().insert(
7025	            "exit-plan-mode-resume".to_string(),
7026	            crate::session::pending_interaction::PendingKind::PlanApproval,
7027	        );
7028	        agent.insert_resident(&sid, handle);
7029	        let mut observed = spawn_fake_actor(cmd_rx, false);
7030	        drive_disconnect(&agent, &sid).await;
7031	        assert!(
7032	            agent.is_resident(&sid),
7033	            "a session with a parked plan-approval must stay resident"
7034	        );
7035	        assert_eq!(
7036	            agent.session_live_state_for(&sid),
7037	            Some(SessionLiveState::Working),
7038	            "a parked-approval session is kept Working"
7039	        );
7040	        tokio::task::yield_now().await;
7041	        assert!(
7042	            matches!(
7043	                observed.try_recv(),
7044	                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
7045	            ),
7046	            "a parked-approval session must not be sent Shutdown"
7047	        );
7048	    });
7049	}
7050	#[test]
7051	fn disconnect_keeps_the_workflow_session_and_evicts_the_idle_one() {
7052	    run_local_for_bridge_test(|| async {
7053	        let agent = build_minimal_agent_for_tests();
7054	        let workflow_sid = acp::SessionId::new("sess-workflow");
7055	        let (wf_handle, _wf_tx, wf_rx) = make_live_session_handle(&workflow_sid, None);
7056	        let active_work = wf_handle.active_work.clone();
7057	        agent.insert_resident(&workflow_sid, wf_handle);
7058	        spawn_active_work_actor(wf_rx, active_work.clone());
7059	        let _workflow = crate::session::handle::WorkGuard::new(active_work.clone());
7060	        let idle_sid = acp::SessionId::new("sess-idle");
7061	        let (idle_handle, _idle_tx, idle_rx) = make_live_session_handle(&idle_sid, None);
7062	        let idle_work = idle_handle.active_work.clone();
7063	        agent.insert_resident(&idle_sid, idle_handle);
7064	        spawn_active_work_actor(idle_rx, idle_work);
7065	        drive_disconnect_many(&agent, &[&workflow_sid, &idle_sid]).await;
7066	        assert!(
7067	            agent.is_resident(&workflow_sid),
7068	            "a session running a workflow must survive client disconnect (GBT-6282)"
7069	        );
7070	        assert!(
7071	            !agent.is_resident(&idle_sid),
7072	            "an idle session must be evicted on client disconnect"
7073	        );
7074	    });
7075	}
7076	/// Mixed batch in a *single* `x.ai/internal/evict_sessions` notification, the realistic disconnect shape.
7077	/// This is the path that exercises `handle_evict_sessions`' `join_all` two-pass (concurrent `IsBusy` checks, then sequential act).
7078	/// One session's actor reports busy (kept resident, `Working`, no `Shutdown`); the other is idle (unloaded, `Dormant`, `Shutdown` sent). Each must get its own outcome with no cross-contamination between the concurrent check pass and the sequential act pass.
7079	#[test]
7080	fn disconnect_mixed_batch_keeps_busy_unloads_idle() {
7081	    run_local_for_bridge_test(|| async {
7082	        let agent = build_minimal_agent_for_tests();
7083	        let sid_busy = acp::SessionId::new("sess-batch-busy");
7084	        let sid_idle = acp::SessionId::new("sess-batch-idle");
7085	        let (busy_handle, _busy_tx, busy_rx) = make_live_session_handle(&sid_busy, None);
7086	        let (idle_handle, _idle_tx, idle_rx) = make_live_session_handle(&sid_idle, None);
7087	        agent.insert_resident(&sid_busy, busy_handle);
7088	        agent.insert_resident(&sid_idle, idle_handle);
7089	        let idle_root = agent
7090	            .session_registry
7091	            .register_root(sid_idle.clone(), test_root_identity(3, 4));
7092	        assert_root_views(&agent, &sid_idle, Some(&idle_root));
7093	        let mut busy_observed = spawn_fake_actor(busy_rx, true);
7094	        let mut idle_observed = spawn_fake_actor(idle_rx, false);
7095	        drive_disconnect_many(&agent, &[&sid_busy, &sid_idle]).await;
7096	        assert!(
7097	            agent.is_resident(&sid_busy),
7098	            "the busy session in the batch must stay resident"
7099	        );
7100	        assert_eq!(
7101	            agent.session_live_state_for(&sid_busy),
7102	            Some(SessionLiveState::Working),
7103	            "the busy session must be Working"
7104	        );
7105	        assert!(
7106	            !agent.is_resident(&sid_idle),
7107	            "the idle session in the batch must be unloaded"
7108	        );
7109	        assert_eq!(
7110	            agent.session_live_state_for(&sid_idle),
7111	            Some(SessionLiveState::Dormant),
7112	            "the idle session must be Dormant"
7113	        );
7114	        assert_root_views(&agent, &sid_idle, None);
7115	        let idle_shutdown =
7116	            tokio::time::timeout(std::time::Duration::from_secs(1), idle_observed.recv())
7117	                .await
7118	                .expect("idle session must receive a command within 1s")
7119	                .expect("fake actor channel must stay open");
7120	        assert!(
7121	            matches!(idle_shutdown, TestSessionCommand::Shutdown(_)),
7122	            "the idle session must be sent Shutdown"
7123	        );
7124	        tokio::task::yield_now().await;
7125	        assert!(
7126	            matches!(
7127	                busy_observed.try_recv(),
7128	                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
7129	            ),
7130	            "the busy session must not be sent Shutdown in a mixed batch"
7131	        );
7132	        assert!(
7133	            agent.finalize_spy.borrow().is_empty(),
7134	            "neither batch outcome may finalize on a mere disconnect"
7135	        );
7136	    });
7137	}
7138	/// The bounded `session_live_state` map does not grow without bound across repeated create/close cycles.
7139	/// Every terminal close drops its entry, so the map size stays at the live count, not the cumulative count.
7140	#[test]
7141	fn session_live_state_map_is_bounded_across_cycles() {
7142	    run_local_for_bridge_test(|| async {
7143	        let agent = build_minimal_agent_for_tests();
7144	        for i in 0..50 {
7145	            let sid = acp::SessionId::new(format!("sess-cycle-{i}"));
7146	            let (handle, _tx, rx) = make_live_session_handle(&sid, Some("turn"));
7147	            agent.insert_resident(&sid, handle);
7148	            let _observed = spawn_fake_actor(rx, true);
7149	            agent.set_session_live_state(&sid, SessionLiveState::IdleResident);
7150	            assert_eq!(
7151	                agent.close_active_session(&sid).await,
7152	                crate::agent::mvp_agent::session_lifecycle::CloseOutcome::Closed,
7153	                "cycle {i} must actually close, or the bound below proves nothing"
7154	            );
7155	        }
7156	        assert_eq!(
7157	            agent.session_registry.counts().session_live_state,
7158	            0,
7159	            "terminal closes must leave no residual live-state entries (bounded map)"
7160	        );
7161	    });
7162	}
7163	/// Finalize fires on a genuine terminal close, driven through the real `x.ai/session/close` dispatch rather than the internal helper.
7164	#[test]
7165	fn explicit_close_finalizes_the_replica() {
7166	    run_local_for_bridge_test(|| async {
7167	        let agent = build_minimal_agent_for_tests();
7168	        let sid = acp::SessionId::new("sess-close");
7169	        let (handle, _tx, cmd_rx) = make_live_session_handle(&sid, Some("turn-1"));
7170	        agent.insert_resident(&sid, handle);
7171	        let mut cmd_rx = spawn_fake_actor(cmd_rx, true);
7172	        drive_close(&agent, "no-such-session")
7173	            .await
7174	            .expect("close of a missing session must succeed as a no-op");
7175	        assert!(
7176	            agent.finalize_spy.borrow().is_empty(),
7177	            "closing a missing session must NOT finalize"
7178	        );
7179	        drive_close(&agent, sid.0.as_ref())
7180	            .await
7181	            .expect("session close must be handled");
7182	        let cmd = tokio::time::timeout(std::time::Duration::from_secs(1), cmd_rx.recv())
7183	            .await
7184	            .expect("close must send Cancel")
7185	            .expect("fake actor channel must stay open");
7186	        let TestSessionCommand::Cancel(options) = cmd else {
7187	            panic!("close must send Cancel before anything else");
7188	        };
7189	        assert_eq!(
7190	            (
7191	                options.cancel_subagents,
7192	                options.kill_background_tasks,
7193	                options.history.clone(),
7194	                options.trigger.as_ref().map(|t| t.as_str()),
7195	                options.user_initiated
7196	            ),
7197	            (
7198	                true,
7199	                true,
7200	                crate::session::CancelHistoryDisposition::Keep,
7201	                Some("session_close"),
7202	                false
7203	            ),
7204	        );
7205	        assert!(
7206	            matches!(
7207	                cmd_rx.try_recv(),
7208	                Ok(TestSessionCommand::Shutdown(
7209	                    crate::session::ShutdownKind::CancelRunningTurn
7210	                ))
7211	            ),
7212	            "close frees the session, so its Shutdown must cancel the running \
7213	             turn; a graceful one would let the turn answer EndTurn as the \
7214	             actor tears down"
7215	        );
7216	        assert_eq!(
7217	            agent.finalize_spy.borrow().as_slice(),
7218	            &[sid.0.to_string()],
7219	            "explicit close must finalize the cloud replica exactly once"
7220	        );
7221	        assert!(
7222	            !agent.is_resident(&sid),
7223	            "explicit close removes the session"
7224	        );
7225	        assert_eq!(
7226	            agent.session_live_state_for(&sid),
7227	            None,
7228	            "terminal removal must drop the live-state entry (bounded map)"
7229	        );
7230	        assert!(
7231	            agent
7232	                .roster_delta_spy
7233	                .borrow()
7234	                .iter()
7235	                .any(|(id, st)| id == sid.0.as_ref() && *st == SessionLiveState::Completed),
7236	            "explicit close must emit a Completed roster delta"
7237	        );
7238	    });
7239	}
7240	/// Join-handle supervisor: a *resident* actor that panics is reaped promptly. It is removed from `sessions`/`session_threads` and demoted to `DeadFailed` (seen via the roster delta; the live-state entry is dropped).
7241	/// It is NOT finalized (the conversation persists). Polls in real time (the panic unwinds on a real OS thread, independent of the tokio clock); the reap lands within a few supervisor ticks.
7242	/// The injected-panic backtrace on stderr is expected and harmless.
7243	#[test]
7244	fn supervisor_reaps_panicked_resident_actor() {
7245	    run_local_for_bridge_test(|| async {
7246	        let agent = build_minimal_agent_for_tests();
7247	        let sid = acp::SessionId::new("sess-panic");
7248	        let (handle, _tx, _rx) = make_live_session_handle(&sid, Some("turn-1"));
7249	        agent.insert_resident(&sid, handle);
7250	        let panic_thread = std::thread::spawn(|| panic!("injected actor panic"));
7251	        agent.session_registry.set_thread(
7252	            &sid,
7253	            crate::session::SessionThread::from_handle(panic_thread),
7254	        );
7255	        agent.set_session_live_state(&sid, SessionLiveState::Working);
7256	        agent.ensure_session_supervisor();
7257	        let deadline = tokio::time::Instant::now() + (SESSION_SUPERVISOR_TICK * 6);
7258	        while tokio::time::Instant::now() < deadline {
7259	            if !agent.session_registry.has_thread(&sid) {
7260	                break;
7261	            }
7262	            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
7263	        }
7264	        assert!(
7265	            !agent.session_registry.has_thread(&sid),
7266	            "supervisor must reap the dead thread"
7267	        );
7268	        assert!(
7269	            !agent.is_resident(&sid),
7270	            "reaped session must be removed from the resident map"
7271	        );
7272	        assert_eq!(
7273	            agent.session_live_state_for(&sid),
7274	            None,
7275	            "terminal removal drops the live-state entry (bounded map)"
7276	        );
7277	        assert!(
7278	            agent
7279	                .roster_delta_spy
7280	                .borrow()
7281	                .iter()
7282	                .any(|(id, st)| id == sid.0.as_ref() && *st == SessionLiveState::DeadFailed),
7283	            "a reaped resident actor must emit a DeadFailed roster delta"
7284	        );
7285	        assert!(
7286	            agent.finalize_spy.borrow().is_empty(),
7287	            "reaping a dead actor must NOT finalize (conversation persists)"
7288	        );
7289	    });
7290	}
7291	/// Regression: writeback must self-correct once remote settings arrive (the field used to be frozen at construction).
7292	#[tokio::test]
7293	#[serial_test::serial]
7294	async fn storage_mode_self_corrects_to_writeback_when_settings_arrive() {
7295	    let _env = crate::env::EnvVarGuard::remove("GROK_STORAGE_MODE");
7296	    let auth = xai_grok_login::GrokAuth {
7297	        auth_mode: xai_grok_login::AuthMode::Oidc,
7298	        oidc_issuer: Some("https://auth.x.ai".to_string()),
7299	        key: "test-token".to_string(),
7300	        ..Default::default()
7301	    };
7302	    let agent = build_agent_with_auth(auth);
7303	    agent.cfg.borrow_mut().mode = crate::agent::config::AgentMode::Leader;
7304	    assert_eq!(agent.storage_mode(), StorageMode::Local);
7305	    agent.cfg.borrow_mut().remote_settings = Some(crate::util::config::RemoteSettings {
7306	        writeback_enabled: Some(true),
7307	        ..Default::default()
7308	    });
7309	    agent.on_remote_settings_changed();
7310	    assert_eq!(agent.storage_mode(), StorageMode::Writeback);
7311	}
7312	/// `spawn_settings_reapply` coalesces: while one reapply is in flight, repeated calls (boot plus rapid `/new`) do not spawn overlapping tasks.
7313	#[test]
7314	fn spawn_settings_reapply_coalesces_while_in_flight() {
7315	    run_local_for_bridge_test(|| async {
7316	        let agent = build_minimal_agent_for_tests();
7317	        assert_eq!(agent.settings_reapply_spawn_count.get(), 0);
7318	        agent.spawn_settings_reapply();
7319	        agent.spawn_settings_reapply();
7320	        agent.spawn_settings_reapply();
7321	        assert_eq!(
7322	            agent.settings_reapply_spawn_count.get(),
7323	            1,
7324	            "overlapping settings reapplies must coalesce to a single task"
7325	        );
7326	        assert!(agent.settings_reapply_in_flight.get());
7327	    });
7328	}
7329	/// The in-flight guard clears on task completion (via the `ClearOnDrop` guard, so it also clears on panic), allowing a later reapply to re-spawn.
7330	#[test]
7331	fn spawn_settings_reapply_clears_flag_after_completion() {
7332	    run_local_for_bridge_test(|| async {
7333	        let agent = build_minimal_agent_for_tests();
7334	        agent.spawn_settings_reapply();
7335	        assert_eq!(agent.settings_reapply_spawn_count.get(), 1);
7336	        assert!(agent.settings_reapply_in_flight.get());
7337	        let mut cleared = false;
7338	        for _ in 0..40 {
7339	            if !agent.settings_reapply_in_flight.get() {
7340	                cleared = true;
7341	                break;
7342	            }
7343	            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
7344	        }
7345	        assert!(
7346	            cleared,
7347	            "in-flight flag must clear after the task completes"
7348	        );
7349	        agent.spawn_settings_reapply();
7350	        assert_eq!(
7351	            agent.settings_reapply_spawn_count.get(),
7352	            2,
7353	            "a reapply after completion must spawn again"
7354	        );
7355	    });
7356	}
7357	/// The post-auth fetch has its own guard.
7358	/// An in-flight settings reapply cannot coalesce away a freshly authenticated identity's gate and settings resolution.
7359	#[test]
7360	fn post_auth_settings_not_coalesced_by_in_flight_reapply() {
7361	    run_local_for_bridge_test(|| async {
7362	        let agent = build_minimal_agent_for_tests();
7363	        agent.spawn_settings_reapply();
7364	        assert!(agent.settings_reapply_in_flight.get());
7365	        agent.spawn_post_auth_settings(xai_grok_login::GrokAuth::test_default());
7366	        assert_eq!(
7367	            agent.post_auth_settings_spawn_count.get(),
7368	            1,
7369	            "post-auth must spawn on its own guard despite an in-flight reapply"
7370	        );
7371	        assert!(agent.post_auth_settings_in_flight.get());
7372	    });
7373	}
7374	/// The tier re-check work is single-flight across every caller: back-to-back gated initializes run at most one live check.
7375	/// An awaited authenticate-path check skips (rather than doubles or waits out) a check already wedged on a stalled subscription endpoint. Drives the exact block `initialize` runs when `tier_allowed` is false.
7376	/// The full `initialize` fires once-per-process GROK_HOME cleanup work that a unit test must not run against the developer's real home.
7377	#[test]
7378	fn gated_reconnect_tier_recheck_is_single_flight() {
7379	    run_local_for_bridge_test(|| async {
7380	        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
7381	        let addr = listener.local_addr().expect("addr");
7382	        listener
7383	            .set_nonblocking(true)
7384	            .expect("nonblocking accept loop");
7385	        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
7386	        let accept_stop = stop.clone();
7387	        let accept_thread = std::thread::spawn(move || {
7388	            let mut held = Vec::new();
7389	            while !accept_stop.load(std::sync::atomic::Ordering::Acquire) {
7390	                match listener.accept() {
7391	                    Ok((stream, _)) => held.push(stream),
7392	                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
7393	                        std::thread::sleep(std::time::Duration::from_millis(20));
7394	                    }
7395	                    Err(_) => break,
7396	                }
7397	            }
7398	        });
7399	        let agent = build_minimal_agent_for_tests();
7400	        agent.cfg.borrow_mut().endpoints.cli_chat_proxy_base_url =
7401	            Some(format!("http://{addr}/v1"));
7402	        agent.cfg.borrow_mut().remote_settings = Some(crate::util::config::RemoteSettings {
7403	            allow_access: Some(false),
7404	            ..Default::default()
7405	        });
7406	        let auth = xai_grok_login::GrokAuth {
7407	            key: "gated-user-key".into(),
7408	            user_id: "user-gated".into(),
7409	            auth_mode: xai_grok_login::AuthMode::Oidc,
7410	            oidc_issuer: Some(xai_grok_login::XAI_OAUTH2_ISSUER.to_owned()),
7411	            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
7412	            ..xai_grok_login::GrokAuth::test_default()
7413	        };
7414	        agent.auth_manager.hot_swap(auth.clone());
7415	        *agent.allow_access_resolved_for.borrow_mut() = Some(auth.user_id.clone());
7416	        agent.tier_allowed.set(false);
7417	        for _ in 0..2 {
7418	            if !agent.tier_allowed.get() {
7419	                agent.spawn_tier_recheck();
7420	            }
7421	            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
7422	        }
7423	        assert_eq!(
7424	            agent.tier_recheck_run_count.get(),
7425	            1,
7426	            "the second spawned check must skip the claimed re-check"
7427	        );
7428	        tokio::time::timeout(
7429	            std::time::Duration::from_secs(2),
7430	            agent.enforce_grok_code_access(&auth),
7431	        )
7432	        .await
7433	        .expect("an awaited check must skip, not wait out, the wedged re-check");
7434	        assert_eq!(
7435	            agent.tier_recheck_run_count.get(),
7436	            1,
7437	            "the awaited check must not run a second concurrent re-check"
7438	        );
7439	        assert!(
7440	            !agent.tier_allowed.get(),
7441	            "the gate stays until a re-check resolves"
7442	        );
7443	        stop.store(true, std::sync::atomic::Ordering::Release);
7444	        accept_thread.join().expect("accept loop joins");
7445	    });
7446	}
7447	/// The check's own mint spawns a `/user` enrichment that can rewrite the in-memory user_id to the proxy-canonical value mid-check.
7448	/// The identity guard must read that normalization as the same account (it is the id the check's own bearer resolved to).
7449	/// A live id matching neither the started nor the canonical id is a real switch and still discards.
7450	#[test]
7451	fn tier_recheck_identity_guard_accepts_enrichment_canonical_user_id() {
7452	    run_local_for_bridge_test(|| async {
7453	        let agent = build_minimal_agent_for_tests();
7454	        let auth = xai_grok_login::GrokAuth {
7455	            key: "seeded-key".into(),
7456	            user_id: "canonical-user".into(),
7457	            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
7458	            ..xai_grok_login::GrokAuth::test_default()
7459	        };
7460	        agent.auth_manager.hot_swap(auth);
7461	        assert!(!agent.tier_recheck_identity_changed("seeded-user", Some("canonical-user")));
7462	        assert!(!agent.tier_recheck_identity_changed("canonical-user", None));
7463	        assert!(!agent.tier_recheck_identity_changed("canonical-user", Some("other-user")));
7464	        assert!(agent.tier_recheck_identity_changed("seeded-user", Some("other-user")));
7465	        assert!(agent.tier_recheck_identity_changed("seeded-user", None));
7466	        assert!(agent.tier_recheck_identity_changed("seeded-user", Some("")));
7467	    });
7468	}
7469	/// The other half of the reconnect paywall flash (the wedged test above locks the "gate holds while the check is in flight" half).
7470	/// A re-check that confirms a qualifying tier lifts `tier_allowed`, so the flash a subscribed user can see on a gated reconnect clears. Settings stay absent (the mock 404s `/settings`).
7471	/// That models the remote-fetch-failed / disabled arm where the confirmed tier is the authority for the lift. The bearer's tier claim already matches the live tier, so the post-unblock mint is skipped and no refresher is needed.
7472	#[test]
7473	fn gated_reconnect_recheck_lifts_gate_clearing_paywall_flash() {
7474	    run_local_for_bridge_test(|| async {
7475	        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
7476	        let addr = listener.local_addr().expect("addr");
7477	        listener
7478	            .set_nonblocking(true)
7479	            .expect("nonblocking accept loop");
7480	        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
7481	        let accept_stop = stop.clone();
7482	        let accept_thread = std::thread::spawn(move || {
7483	            use std::io::{Read, Write};
7484	            while !accept_stop.load(std::sync::atomic::Ordering::Acquire) {
7485	                match listener.accept() {
7486	                    Ok((mut stream, _)) => {
7487	                        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
7488	                        let mut head = Vec::new();
7489	                        let mut byte = [0u8; 1];
7490	                        while !head.ends_with(b"\r\n\r\n") {
7491	                            match stream.read(&mut byte) {
7492	                                Ok(1) => head.push(byte[0]),
7493	                                _ => break,
7494	                            }
7495	                        }
7496	                        let head = String::from_utf8_lossy(&head);
7497	                        let response = if head.contains("/user?include=subscription") {
7498	                            let body =
7499	                                r#"{"userId":"user-flash","subscriptionTier":"SuperGrokPro"}"#;
7500	                            format!(
7501	                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
7502	                                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
7503	                                body.len()
7504	                            )
7505	                        } else {
7506	                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\
7507	                             Connection: close\r\n\r\n"
7508	                                .to_owned()
7509	                        };
7510	                        let _ = stream.write_all(response.as_bytes());
7511	                    }
7512	                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
7513	                        std::thread::sleep(std::time::Duration::from_millis(20));
7514	                    }
7515	                    Err(_) => break,
7516	                }
7517	            }
7518	        });
7519	        let temp_dir = tempfile::tempdir().unwrap();
7520	        let auth_manager = std::sync::Arc::new(xai_grok_login::AuthManager::new(
7521	            temp_dir.path(),
7522	            xai_grok_login::GrokComConfig::default(),
7523	        ));
7524	        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
7525	        let gateway = GatewaySender::new(tx);
7526	        let cfg = crate::agent::config::Config::default();
7527	        let agent =
7528	            MvpAgent::new(gateway, &cfg, auth_manager, None, None).expect("valid test config");
7529	        agent.cfg.borrow_mut().endpoints.cli_chat_proxy_base_url =
7530	            Some(format!("http://{addr}/v1"));
7531	        let auth = xai_grok_login::GrokAuth {
7532	            key: jwt_with_tier(5),
7533	            user_id: "user-flash".into(),
7534	            auth_mode: xai_grok_login::AuthMode::Oidc,
7535	            oidc_issuer: Some(xai_grok_login::XAI_OAUTH2_ISSUER.to_owned()),
7536	            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
7537	            ..xai_grok_login::GrokAuth::test_default()
7538	        };
7539	        agent.auth_manager.hot_swap(auth);
7540	        agent.tier_allowed.set(false);
7541	        tokio::time::timeout(
7542	            std::time::Duration::from_secs(30),
7543	            agent.retry_subscription_check(),
7544	        )
7545	        .await
7546	        .expect("re-check must finish well inside its bounded awaits");
7547	        assert!(
7548	            agent.tier_allowed.get(),
7549	            "a confirmed qualifying tier must lift the gate — the reconnect paywall flash clears"
7550	        );
7551	        assert_eq!(
7552	            agent.tier_recheck_run_count.get(),
7553	            1,
7554	            "the lift came from exactly one claimed re-check"
7555	        );
7556	        stop.store(true, std::sync::atomic::Ordering::Release);
7557	        accept_thread.join().expect("accept loop joins");
7558	    });
7559	}
7560	/// Agent with pre-loaded auth, a gateway receiver (to assert emitted notifications), and the proxy URL pointed at a mock `/v1/settings`.
7561	pub(super) fn build_agent_with_auth_and_proxy(
7562	    auth: xai_grok_login::GrokAuth,
7563	    proxy_url: String,
7564	    mode: crate::agent::config::AgentMode,
7565	) -> (
7566	    MvpAgent,
7567	    tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
7568	) {
7569	    use crate::agent::config::Config as AgentConfig;
7570	    use xai_grok_login::{AuthManager, GrokComConfig};
7571	    let temp_dir = tempfile::tempdir().unwrap();
7572	    let auth_manager =
7573	        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
7574	    auth_manager.hot_swap(auth);
7575	    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
7576	    let gateway = GatewaySender::new(tx);
7577	    let mut cfg = AgentConfig {
7578	        mode,
7579	        ..Default::default()
7580	    };
7581	    cfg.endpoints.cli_chat_proxy_base_url = Some(proxy_url);
7582	    let agent = MvpAgent::new(gateway, &cfg, auth_manager, None, None).expect("valid test config");
7583	    (agent, rx)
7584	}
7585	/// The manager's proxy also points at the mock (the `/user` fetch uses it) and the credential is on disk. The `TempDir` must outlive the agent or `auth.json` writes fail.
7586	async fn build_hydration_agent(
7587	    proxy_url: &str,
7588	    auth: xai_grok_login::GrokAuth,
7589	) -> (MvpAgent, tempfile::TempDir) {
7590	    use crate::agent::config::{AgentMode, Config as AgentConfig};
7591	    use xai_grok_login::{AuthManager, GrokComConfig};
7592	    let temp_dir = tempfile::tempdir().unwrap();
7593	    let auth_manager = std::sync::Arc::new(
7594	        AuthManager::new(temp_dir.path(), GrokComConfig::default()).with_proxy_base_url(proxy_url),
7595	    );
7596	    auth_manager.save_without_enrichment(auth).await.unwrap();
7597	    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
7598	    let mut cfg = AgentConfig {
7599	        mode: AgentMode::Leader,
7600	        ..Default::default()
7601	    };
7602	    cfg.endpoints.cli_chat_proxy_base_url = Some(proxy_url.to_owned());
7603	    let agent = MvpAgent::new(GatewaySender::new(tx), &cfg, auth_manager, None, None)
7604	        .expect("valid test config");
7605	    (agent, temp_dir)
7606	}
7607	const USER_A: &str = "a@acme.test";
7608	const USER_B: &str = "b@acme.test";
7609	/// A nameless Team principal: the claim lands at login while `team_name` waits on `/user`.
7610	fn team_auth() -> xai_grok_login::GrokAuth {
7611	    xai_grok_login::GrokAuth {
7612	        oidc_issuer: Some(xai_grok_login::XAI_OAUTH2_ISSUER.to_string()),
7613	        email: Some(USER_A.into()),
7614	        principal_type: Some(xai_grok_login::model::TEAM_PRINCIPAL_TYPE.into()),
7615	        team_id: Some("team-a".into()),
7616	        ..xai_grok_login::GrokAuth::test_default()
7617	    }
7618	}
7619	async fn hydrate(agent: &MvpAgent, email: &str) -> Option<bool> {
7620	    let req = acp::ExtRequest::new(
7621	        "x.ai/auth/hydrate_team_capability",
7622	        serde_json::value::to_raw_value(&serde_json::json!({ "email": email, "teamId": "team-a" }))
7623	            .unwrap()
7624	            .into(),
7625	    );
7626	    let resp = crate::extensions::auth::handle(agent, &req).await.unwrap();
7627	    let body: serde_json::Value = serde_json::from_str(resp.0.get()).unwrap();
7628	    assert!(
7629	        body.get("canAdministerTeam").is_some() && body.get("codingDataRetentionOptOut").is_none(),
7630	        "{body}"
7631	    );
7632	    body.get("canAdministerTeam").and_then(|v| v.as_bool())
7633	}
7634	/// A `/user` answer parked past a Settings opt-out still carries the opted-in value; only the capability may land, and only in memory.
7635	#[tokio::test]
7636	async fn hydration_delayed_past_an_opt_out_keeps_the_opt_out() {
7637	    let server = xai_grok_test_support::MockInferenceServer::start()
7638	        .await
7639	        .unwrap();
7640	    server.set_user_can_administer_team(xai_grok_test_support::MockCanAdministerTeam::Denied);
7641	    server.set_user_coding_data_retention_opt_out(Some(false));
7642	    server.hold_user_info();
7643	    let (agent, _home) = build_hydration_agent(
7644	        &server.url(),
7645	        xai_grok_login::GrokAuth {
7646	            coding_data_retention_opt_out: false,
7647	            ..team_auth()
7648	        },
7649	    )
7650	    .await;
7651	    let opt_out_then_release = async {
7652	        server.user_info_arrived(1).await;
7653	        let mut opted_out = agent.auth_manager.current().unwrap();
7654	        opted_out.coding_data_retention_opt_out = true;
7655	        agent
7656	            .auth_manager
7657	            .save_without_enrichment(opted_out)
7658	            .await
7659	            .unwrap();
7660	        server.release_user_info();
7661	    };
7662	    let (capability, ()) = tokio::join!(hydrate(&agent, USER_A), opt_out_then_release);
7663	    assert_eq!(capability, Some(false));
7664	    let live = agent.auth_manager.current().unwrap();
7665	    assert_eq!(live.can_administer_team, Some(false));
7666	    assert!(live.coding_data_retention_opt_out);
7667	    let disk = xai_grok_login::storage::read_auth_json(agent.auth_manager.auth_json_path())
7668	        .unwrap()
7669	        .into_values()
7670	        .next()
7671	        .unwrap();
7672	    assert_eq!(disk.can_administer_team, None);
7673	    assert!(disk.coding_data_retention_opt_out);
7674	}
7675	/// An answer for account A never reaches account B: not a GET parked past the switch, not a request still naming A, and not B's cached value.
7676	#[tokio::test]
7677	async fn hydration_is_bound_to_the_account_that_asked() {
7678	    let server = xai_grok_test_support::MockInferenceServer::start()
7679	        .await
7680	        .unwrap();
7681	    server.set_user_can_administer_team(xai_grok_test_support::MockCanAdministerTeam::Denied);
7682	    server.hold_user_info();
7683	    let (agent, _home) = build_hydration_agent(&server.url(), team_auth()).await;
7684	    let switch_then_release = async {
7685	        server.user_info_arrived(1).await;
7686	        agent.auth_manager.hot_swap(xai_grok_login::GrokAuth {
7687	            key: "key-b".into(),
7688	            email: Some(USER_B.into()),
7689	            ..team_auth()
7690	        });
7691	        server.release_user_info();
7692	    };
7693	    let (late_answer, ()) = tokio::join!(hydrate(&agent, USER_A), switch_then_release);
7694	    assert_eq!(late_answer, None);
7695	    assert_eq!(
7696	        agent.auth_manager.current().unwrap().can_administer_team,
7697	        None
7698	    );
7699	    assert_eq!(hydrate(&agent, USER_A).await, None);
7700	    assert_eq!(server.request_count_for("/v1/user"), 1);
7701	    assert_eq!(hydrate(&agent, USER_B).await, Some(false));
7702	    assert_eq!(hydrate(&agent, USER_A).await, None);
7703	    assert_eq!(hydrate(&agent, USER_B).await, Some(false));
7704	    assert_eq!(server.request_count_for("/v1/user"), 2);
7705	}
7706	/// A token refresh mid-GET rotates the bearer without changing who asked, so the answer lands; one its enrichment already resolved wins and is what the caller hears; a User principal on the same team is another account.
7707	#[tokio::test]
7708	async fn hydration_merges_only_into_the_same_unresolved_principal() {
7709	    let rotated = || xai_grok_login::GrokAuth {
7710	        key: "key-rotated".into(),
7711	        ..team_auth()
7712	    };
7713	    for (swapped_in, answer, live_after) in [
7714	        (rotated(), Some(false), Some(false)),
7715	        (
7716	            xai_grok_login::GrokAuth {
7717	                can_administer_team: Some(true),
7718	                ..rotated()
7719	            },
7720	            Some(true),
7721	            Some(true),
7722	        ),
7723	        (
7724	            xai_grok_login::GrokAuth {
7725	                principal_type: Some("User".into()),
7726	                ..rotated()
7727	            },
7728	            None,
7729	            None,
7730	        ),
7731	    ] {
7732	        let server = xai_grok_test_support::MockInferenceServer::start()
7733	            .await
7734	            .unwrap();
7735	        server.set_user_can_administer_team(xai_grok_test_support::MockCanAdministerTeam::Denied);
7736	        server.hold_user_info();
7737	        let (agent, _home) = build_hydration_agent(&server.url(), team_auth()).await;
7738	        let swap_then_release = async {
7739	            server.user_info_arrived(1).await;
7740	            agent.auth_manager.hot_swap(swapped_in);
7741	            server.release_user_info();
7742	        };
7743	        let (capability, ()) = tokio::join!(hydrate(&agent, USER_A), swap_then_release);
7744	        assert_eq!(capability, answer);
7745	        assert_eq!(
7746	            agent.auth_manager.current().unwrap().can_administer_team,
7747	            live_after
7748	        );
7749	    }
7750	}
7751	/// Two pagers ask while the cache is unknown; the second answer finds it resolved and must still tell its caller.
7752	#[tokio::test]
7753	async fn concurrent_hydrations_both_hear_the_resolved_capability() {
7754	    let server = xai_grok_test_support::MockInferenceServer::start()
7755	        .await
7756	        .unwrap();
7757	    server.set_user_can_administer_team(xai_grok_test_support::MockCanAdministerTeam::Denied);
7758	    server.hold_user_info();
7759	    let (agent, _home) = build_hydration_agent(&server.url(), team_auth()).await;
7760	    let release = async {
7761	        server.user_info_arrived(2).await;
7762	        server.release_user_info();
7763	    };
7764	    let (a, b, ()) = tokio::join!(hydrate(&agent, USER_A), hydrate(&agent, USER_A), release);
7765	    assert_eq!((a, b), (Some(false), Some(false)));
7766	}
7767	/// A fetch that fails or answers no capability, null or omitted, leaves the credential exactly as it was.
7768	#[tokio::test]
7769	async fn unresolved_or_failed_hydration_changes_nothing() {
7770	    let opted_out = || xai_grok_login::GrokAuth {
7771	        coding_data_retention_opt_out: true,
7772	        ..team_auth()
7773	    };
7774	    let untouched = |agent: &MvpAgent| {
7775	        let live = agent.auth_manager.current().unwrap();
7776	        (live.can_administer_team, live.coding_data_retention_opt_out)
7777	    };
7778	    let server = xai_grok_test_support::MockInferenceServer::start()
7779	        .await
7780	        .unwrap();
7781	    server.set_user_can_administer_team(xai_grok_test_support::MockCanAdministerTeam::Unresolved);
7782	    server.set_user_coding_data_retention_opt_out(Some(false));
7783	    let (agent, _home) = build_hydration_agent(&server.url(), opted_out()).await;
7784	    assert_eq!(hydrate(&agent, USER_A).await, None);
7785	    assert_eq!(untouched(&agent), (None, true));
7786	    assert_eq!(server.request_count_for("/v1/user"), 1);
7787	    server.set_user_can_administer_team(xai_grok_test_support::MockCanAdministerTeam::Omitted);
7788	    assert_eq!(hydrate(&agent, USER_A).await, None);
7789	    assert_eq!(untouched(&agent), (None, true));
7790	    assert_eq!(server.request_count_for("/v1/user"), 2);
7791	    let (agent, _home) = build_hydration_agent("http://127.0.0.1:1/v1", opted_out()).await;
7792	    assert_eq!(hydrate(&agent, USER_A).await, None);
7793	    assert_eq!(untouched(&agent), (None, true));
7794	}
7795	/// `/user` never resolves the capability for a User principal, so a personal account is not fetched even though its credential carries a `team_id`.
7796	#[tokio::test]
7797	async fn personal_account_does_not_fetch_the_team_capability() {
7798	    let server = xai_grok_test_support::MockInferenceServer::start()
7799	        .await
7800	        .unwrap();
7801	    server.set_user_can_administer_team(xai_grok_test_support::MockCanAdministerTeam::Denied);
7802	    let (agent, _home) = build_hydration_agent(
7803	        &server.url(),
7804	        xai_grok_login::GrokAuth {
7805	            principal_type: Some("User".into()),
7806	            ..team_auth()
7807	        },
7808	    )
7809	    .await;
7810	    assert_eq!(hydrate(&agent, USER_A).await, None);
7811	    assert_eq!(server.request_count_for("/v1/user"), 0);
7812	}
7813	/// Drain the gateway, returning `true` if any `x.ai/settings/update` notification was emitted (and acking each so the sender doesn't warn).
7814	fn drained_settings_update(
7815	    rx: &mut tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
7816	) -> bool {
7817	    let mut found = false;
7818	    while let Ok(msg) = rx.try_recv() {
7819	        if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg {
7820	            if &*args.request.method == "x.ai/settings/update" {
7821	                found = true;
7822	            }
7823	            let _ = args.response_tx.send(Ok(()));
7824	        }
7825	    }
7826	    found
7827	}
7828	/// Re-open the process-global external-OTEL gate on drop so a closed gate never leaks into another test.
7829	struct RestoreOtelGate;
7830	impl Drop for RestoreOtelGate {
7831	    fn drop(&mut self) {
7832	        xai_grok_telemetry::external::mark_external_otel_settings_resolved();
7833	    }
7834	}
7835	/// Regression: `cfg.remote_settings` is not reset on an account switch, so the access gate must not read a previous identity's cached `allow_access`.
7836	/// A mismatched identity stays provisionally open (unknown), like the OTEL gate's `rearm_on_switch`.
7837	#[tokio::test]
7838	async fn access_gate_does_not_leak_verdict_across_identities() {
7839	    use crate::agent::config::AgentMode;
7840	    use xai_grok_login::{GrokAuth, XAI_OAUTH2_ISSUER};
7841	    let auth_a = GrokAuth {
7842	        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
7843	        user_id: "user-a".into(),
7844	        ..GrokAuth::test_default()
7845	    };
7846	    let (agent, _rx) = build_agent_with_auth_and_proxy(
7847	        auth_a,
7848	        "http://127.0.0.1:1/".to_string(),
7849	        AgentMode::Leader,
7850	    );
7851	    {
7852	        let mut cfg = agent.cfg.borrow_mut();
7853	        cfg.remote_settings = Some(crate::util::config::RemoteSettings {
7854	            allow_access: Some(false),
7855	            ..Default::default()
7856	        });
7857	    }
7858	    *agent.allow_access_resolved_for.borrow_mut() = Some("user-a".to_string());
7859	    let auth_b = GrokAuth {
7860	        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
7861	        user_id: "user-b".into(),
7862	        ..GrokAuth::test_default()
7863	    };
7864	    assert!(auth_b.is_xai_auth(), "precondition: first-party xAI auth");
7865	    agent.enforce_grok_code_access(&auth_b).await;
7866	    assert!(
7867	        agent.tier_allowed.get(),
7868	        "identity B must not inherit identity A's denied allow_access verdict",
7869	    );
7870	}
7871	/// First-party xAI auth with `writeback_enabled` settings upgrades storage to Writeback.
7872	/// The settings arrival also emits `x.ai/settings/update` and opens the external-OTEL gate.
7873	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
7874	#[serial_test::serial]
7875	async fn post_auth_settings_xai_upgrades_writeback_emits_and_opens_gate() {
7876	    use crate::agent::config::AgentMode;
7877	    use xai_grok_login::{GrokAuth, XAI_OAUTH2_ISSUER};
7878	    let _restore = RestoreOtelGate;
7879	    let _storage_env = crate::env::EnvVarGuard::remove("GROK_STORAGE_MODE");
7880	    let server = xai_grok_test_support::MockInferenceServer::start()
7881	        .await
7882	        .unwrap();
7883	    server.set_settings(serde_json::json!({
7884	        "writeback_enabled": true,
7885	        "allow_access": true,
7886	    }));
7887	    let xai_auth = GrokAuth {
7888	        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
7889	        ..GrokAuth::test_default()
7890	    };
7891	    assert!(xai_auth.is_xai_auth(), "precondition: first-party xAI auth");
7892	    let (agent, mut rx) =
7893	        build_agent_with_auth_and_proxy(xai_auth, server.url(), AgentMode::Leader);
7894	    assert_eq!(
7895	        agent.storage_mode(),
7896	        StorageMode::Local,
7897	        "precondition: leader boots in Local storage mode"
7898	    );
7899	    xai_grok_telemetry::external::suppress_external_otel_until_settings();
7900	    assert!(!xai_grok_telemetry::external::is_settings_gate_open());
7901	    agent.maybe_fetch_post_auth_settings().await;
7902	    assert_eq!(
7903	        agent.storage_mode(),
7904	        StorageMode::Writeback,
7905	        "xai auth + writeback_enabled settings must upgrade storage to Writeback"
7906	    );
7907	    assert!(
7908	        xai_grok_telemetry::external::is_settings_gate_open(),
7909	        "a settings response must open the external-OTEL gate"
7910	    );
7911	    assert!(
7912	        drained_settings_update(&mut rx),
7913	        "settings arrival must push x.ai/settings/update to clients"
7914	    );
7915	}
7916	/// BYOK auth must not be upgraded to `Writeback` even when the server advertises it; the push and gate still fire.
7917	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
7918	#[serial_test::serial]
7919	async fn post_auth_settings_non_xai_keeps_local_but_still_emits() {
7920	    use crate::agent::config::AgentMode;
7921	    use xai_grok_login::{AuthMode, GrokAuth};
7922	    let _restore = RestoreOtelGate;
7923	    let server = xai_grok_test_support::MockInferenceServer::start()
7924	        .await
7925	        .unwrap();
7926	    server.set_settings(serde_json::json!({
7927	        "writeback_enabled": true,
7928	        "allow_access": true,
7929	    }));
7930	    let api_auth = GrokAuth {
7931	        auth_mode: AuthMode::ApiKey,
7932	        ..GrokAuth::test_default()
7933	    };
7934	    assert!(
7935	        !api_auth.is_xai_auth(),
7936	        "precondition: non-first-party auth"
7937	    );
7938	    let (agent, mut rx) =
7939	        build_agent_with_auth_and_proxy(api_auth, server.url(), AgentMode::Leader);
7940	    xai_grok_telemetry::external::suppress_external_otel_until_settings();
7941	    agent.maybe_fetch_post_auth_settings().await;
7942	    assert_eq!(
7943	        agent.storage_mode(),
7944	        StorageMode::Local,
7945	        "non-xai auth must stay Local even when writeback is advertised remotely"
7946	    );
7947	    assert!(
7948	        xai_grok_telemetry::external::is_settings_gate_open(),
7949	        "a settings response must open the gate regardless of auth kind"
7950	    );
7951	    assert!(
7952	        drained_settings_update(&mut rx),
7953	        "settings arrival must push x.ai/settings/update for non-xai auth too"
7954	    );
7955	}
7956	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
7957	#[serial_test::serial]
7958	async fn post_auth_settings_failure_resolves_gate_onto_local_policy() {
7959	    use crate::agent::config::AgentMode;
7960	    use xai_grok_login::{GrokAuth, XAI_OAUTH2_ISSUER};
7961	    let _restore = RestoreOtelGate;
7962	    let server = xai_grok_test_support::MockInferenceServer::start()
7963	        .await
7964	        .unwrap();
7965	    let xai_auth = GrokAuth {
7966	        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
7967	        ..GrokAuth::test_default()
7968	    };
7969	    let (agent, _rx) = build_agent_with_auth_and_proxy(xai_auth, server.url(), AgentMode::Leader);
7970	    xai_grok_telemetry::external::suppress_external_otel_until_settings();
7971	    assert!(!xai_grok_telemetry::external::is_settings_gate_open());
7972	    agent.maybe_fetch_post_auth_settings().await;
7973	    assert!(
7974	        xai_grok_telemetry::external::is_settings_gate_open(),
7975	        "an exhausted fetch is a definitive answer: open on local policy"
7976	    );
7977	    assert!(
7978	        agent.cfg.borrow().remote_settings.is_none(),
7979	        "opening the gate must not fabricate settings; none were fetched"
7980	    );
7981	}
7982	/// A same-credential refresh must NOT re-suppress a gate already resolved for that credential; the reason `OtelGate` remembers the identity.
7983	/// With the gate resolved-open for this identity, a later failing (`Retry`) refresh leaves it OPEN.
7984	/// Regressing the identity guard would re-close it forever.
7985	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
7986	#[serial_test::serial]
7987	async fn same_credential_refresh_does_not_flap_resolved_gate() {
7988	    use crate::agent::config::AgentMode;
7989	    use xai_grok_login::{GrokAuth, XAI_OAUTH2_ISSUER};
7990	    let _restore = RestoreOtelGate;
7991	    let server = xai_grok_test_support::MockInferenceServer::start()
7992	        .await
7993	        .unwrap();
7994	    let xai_auth = GrokAuth {
7995	        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
7996	        ..GrokAuth::test_default()
7997	    };
7998	    let (agent, _rx) =
7999	        build_agent_with_auth_and_proxy(xai_auth.clone(), server.url(), AgentMode::Leader);
8000	    agent.otel_gate.set_resolved_for(&xai_auth.user_id);
8001	    xai_grok_telemetry::external::mark_external_otel_settings_resolved();
8002	    assert!(xai_grok_telemetry::external::is_settings_gate_open());
8003	    agent.refresh_remote_settings(&xai_auth).await;
8004	    assert!(
8005	        xai_grok_telemetry::external::is_settings_gate_open(),
8006	        "a same-credential refresh must not flap a gate already resolved for it"
8007	    );
8008	}
8009	/// A `/settings` 401 from a token that rotated mid-flight must self-heal: refresh once and, if the token changed, re-fetch with it.
8010	/// Without the re-fetch the stale 401 fails OPEN (no remote policy).
8011	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
8012	#[serial_test::serial]
8013	async fn settings_self_heal_refetches_after_token_rotation() {
8014	    use crate::agent::config::AgentMode;
8015	    use xai_grok_login::refresh::{RefreshOutcome, TokenRefresher};
8016	    use xai_grok_login::{GrokAuth, XAI_OAUTH2_ISSUER};
8017	    let _restore = RestoreOtelGate;
8018	    let server = xai_grok_test_support::MockInferenceServer::start_with_required_auth(
8019	        vec![xai_grok_test_support::MockModelEntry::new("grok-build")],
8020	        "rotated-key",
8021	    )
8022	    .await
8023	    .unwrap();
8024	    server.set_settings(serde_json::json!({ "allow_access": true }));
8025	    struct RotatingRefresher;
8026	    #[async_trait::async_trait]
8027	    impl TokenRefresher for RotatingRefresher {
8028	        async fn refresh(&self, _r: xai_grok_login::manager::RefreshReason) -> RefreshOutcome {
8029	            RefreshOutcome::Success(Box::new(GrokAuth {
8030	                key: "rotated-key".into(),
8031	                oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
8032	                refresh_token: Some("rt".into()),
8033	                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
8034	                ..GrokAuth::test_default()
8035	            }))
8036	        }
8037	    }
8038	    let stale = GrokAuth {
8039	        key: "stale-key".into(),
8040	        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
8041	        refresh_token: Some("rt".into()),
8042	        expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
8043	        ..GrokAuth::test_default()
8044	    };
8045	    let (agent, _rx) =
8046	        build_agent_with_auth_and_proxy(stale.clone(), server.url(), AgentMode::Leader);
8047	    agent
8048	        .auth_manager
8049	        .set_refresher(std::sync::Arc::new(RotatingRefresher));
8050	    xai_grok_telemetry::external::suppress_external_otel_until_settings();
8051	    agent.refresh_remote_settings(&stale).await;
8052	    assert!(
8053	        xai_grok_telemetry::external::is_settings_gate_open(),
8054	        "the rotated-token re-fetch must land settings and open the gate"
8055	    );
8056	    assert!(
8057	        agent.cfg.borrow().remote_settings.is_some(),
8058	        "the re-fetched settings must be stored"
8059	    );
8060	}
8061	/// A logout can land while the detached post-auth fetch is in flight; the result must not be cached for the logged-out identity.
8062	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
8063	#[serial_test::serial]
8064	async fn settings_not_cached_when_identity_logs_out_during_fetch() {
8065	    use crate::agent::config::AgentMode;
8066	    use xai_grok_login::{GrokAuth, XAI_OAUTH2_ISSUER};
8067	    let _restore = RestoreOtelGate;
8068	    let server = xai_grok_test_support::MockInferenceServer::start()
8069	        .await
8070	        .unwrap();
8071	    server.set_settings(serde_json::json!({ "allow_access": true }));
8072	    let xai_auth = GrokAuth {
8073	        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
8074	        ..GrokAuth::test_default()
8075	    };
8076	    let (agent, _rx) =
8077	        build_agent_with_auth_and_proxy(xai_auth.clone(), server.url(), AgentMode::Leader);
8078	    agent.auth_manager.clear_in_memory();
8079	    agent.refresh_remote_settings(&xai_auth).await;
8080	    assert!(
8081	        agent.cfg.borrow().remote_settings.is_none(),
8082	        "settings fetched for a logged-out identity must not be cached"
8083	    );
8084	}
8085	/// `ensure_session_supervisor` is idempotent: calling it repeatedly spawns the sweeper loop exactly once.
8086	#[test]
8087	fn ensure_session_supervisor_is_idempotent() {
8088	    run_local_for_bridge_test(|| async {
8089	        let agent = build_minimal_agent_for_tests();
8090	        assert_eq!(agent.supervisor_spawn_count.get(), 0);
8091	        agent.ensure_session_supervisor();
8092	        agent.ensure_session_supervisor();
8093	        agent.ensure_session_supervisor();
8094	        assert_eq!(
8095	            agent.supervisor_spawn_count.get(),
8096	            1,
8097	            "the supervisor task must be spawned at most once"
8098	        );
8099	        assert!(agent.supervisor_started.get());
8100	    });
8101	}
8102	/// After a terminal removal (reap/close drops the live-state entry), a later reload of the same SessionId starts clean at `IdleResident`.
8103	/// No stale terminal state leaks in (ties to the bounded-map fix).
8104	#[test]
8105	fn reload_after_terminal_removal_starts_clean() {
8106	    run_local_for_bridge_test(|| async {
8107	        let agent = build_minimal_agent_for_tests();
8108	        let sid = acp::SessionId::new("sess-reload");
8109	        let (handle, _tx, rx) = make_live_session_handle(&sid, Some("turn-1"));
8110	        agent.insert_resident(&sid, handle);
8111	        let _observed = spawn_fake_actor(rx, true);
8112	        assert_eq!(
8113	            agent.close_active_session(&sid).await,
8114	            crate::agent::mvp_agent::session_lifecycle::CloseOutcome::Closed,
8115	            "the reload below is only meaningful after a close that happened"
8116	        );
8117	        assert_eq!(
8118	            agent.session_live_state_for(&sid),
8119	            None,
8120	            "terminal removal must leave no stale state"
8121	        );
8122	        let (handle2, _tx2, _rx2) = make_live_session_handle(&sid, None);
8123	        agent.insert_resident(&sid, handle2);
8124	        agent.set_session_live_state(&sid, SessionLiveState::IdleResident);
8125	        assert_eq!(
8126	            agent.session_live_state_for(&sid),
8127	            Some(SessionLiveState::IdleResident),
8128	            "a reloaded session must start at IdleResident, not a stale terminal state"
8129	        );
8130	    });
8131	}
8132	/// Build an agent whose gateway is wired to a live receiver.
8133	/// A test can observe (and answer) agent-to-client reverse-requests like the dormant `x.ai/folder_trust/request` round-trip.
8134	fn build_agent_with_gateway_rx() -> (
8135	    MvpAgent,
8136	    tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
8137	) {
8138	    use crate::agent::config::Config as AgentConfig;
8139	    use xai_grok_login::{AuthManager, GrokComConfig};
8140	    let temp_dir = tempfile::tempdir().unwrap();
8141	    let auth_manager =
8142	        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
8143	    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
8144	    let gateway = GatewaySender::new(tx);
8145	    let cfg = AgentConfig::default();
8146	    let agent = MvpAgent::new(gateway, &cfg, auth_manager, None, None).expect("valid test config");
8147	    (agent, rx)
8148	}
8149	/// A git repo whose only repo-local config is a project `.mcp.json` declaring `projsrv`, so it is untrusted-with-configs.
8150	/// The project server should reappear after a trust grant.
8151	fn repo_with_project_mcp_server() -> tempfile::TempDir {
8152	    let tmp = tempfile::tempdir().unwrap();
8153	    git2::Repository::init(tmp.path()).unwrap();
8154	    std::fs::write(
8155	        tmp.path().join(".mcp.json"),
8156	        r#"{"mcpServers":{"projsrv":{"command":"echo","args":["hi"]}}}"#,
8157	    )
8158	    .unwrap();
8159	    tmp
8160	}
8161	fn write_project_subagent_definitions(cwd: &std::path::Path) {
8162	    let roles = cwd.join(".grok/roles");
8163	    let personas = cwd.join(".grok/personas");
8164	    std::fs::create_dir_all(&roles).unwrap();
8165	    std::fs::create_dir_all(&personas).unwrap();
8166	    std::fs::write(roles.join("probe.toml"), "description = \"Project role\"").unwrap();
8167	    std::fs::write(
8168	        personas.join("probe.toml"),
8169	        "instructions = \"Project persona\"",
8170	    )
8171	    .unwrap();
8172	}
8173	fn folder_trust_on() -> crate::util::config::RemoteSettings {
8174	    crate::util::config::RemoteSettings {
8175	        folder_trust_enabled: Some(true),
8176	        ..Default::default()
8177	    }
8178	}
8179	#[test]
8180	#[serial_test::serial]
8181	fn subagent_spawn_context_reloads_project_definitions_after_trust_changes() {
8182	    let repo = tempfile::tempdir().unwrap();
8183	    git2::Repository::init(repo.path()).unwrap();
8184	    write_project_subagent_definitions(repo.path());
8185	    run_local_for_bridge_test(|| async {
8186	        let (agent, _rx) = build_agent_with_gateway_rx();
8187	        let sid = acp::SessionId::new("roles-personas-trust-transition");
8188	        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
8189	        handle.info.cwd = repo.path().display().to_string();
8190	        agent.insert_resident(&sid, handle);
8191	        {
8192	            let mut cfg = agent.cfg.borrow_mut();
8193	            cfg.subagent_roles.insert(
8194	                "refreshed".into(),
8195	                xai_grok_subagent_resolution::config::SubagentRole {
8196	                    description: "Refreshed user role".into(),
8197	                    source_dir: Some(repo.path().join("user-roles")),
8198	                    ..Default::default()
8199	                },
8200	            );
8201	            cfg.subagent_model_overrides
8202	                .insert("probe".into(), "refreshed-model".into());
8203	            cfg.subagent_toggle.insert("probe".into(), false);
8204	        }
8205	        crate::agent::folder_trust::record_for_test(repo.path(), false);
8206	        let untrusted = agent.build_subagent_spawn_context(sid.0.as_ref());
8207	        assert!(!untrusted.subagent_roles.contains_key("probe"));
8208	        assert!(!untrusted.subagent_personas.contains_key("probe"));
8209	        assert_eq!(
8210	            untrusted
8211	                .subagent_roles
8212	                .get("refreshed")
8213	                .map(|role| role.description.as_str()),
8214	            Some("Refreshed user role")
8215	        );
8216	        assert_eq!(
8217	            untrusted
8218	                .subagent_model_overrides
8219	                .get("probe")
8220	                .map(String::as_str),
8221	            Some("refreshed-model")
8222	        );
8223	        assert_eq!(untrusted.subagent_toggle.get("probe"), Some(&false));
8224	        crate::agent::folder_trust::record_for_test(repo.path(), true);
8225	        let trusted = agent.build_subagent_spawn_context(sid.0.as_ref());
8226	        assert_eq!(
8227	            trusted
8228	                .subagent_roles
8229	                .get("probe")
8230	                .map(|role| role.description.as_str()),
8231	            Some("Project role")
8232	        );
8233	        assert!(trusted.subagent_personas.contains_key("probe"));
8234	        crate::agent::folder_trust::record_for_test(repo.path(), false);
8235	        let revoked = agent.build_subagent_spawn_context(sid.0.as_ref());
8236	        assert!(!revoked.subagent_roles.contains_key("probe"));
8237	        assert!(!revoked.subagent_personas.contains_key("probe"));
8238	    });
8239	}
8240	/// End-to-end gate wiring: project `.grok/roles` / `personas` alone must drive the real `resolve_and_record` untrusted.
8241	/// No forced `record_for_test` verdict.
8242	/// Project defs stay out of Task spawn context, then are re-admitted after grant.
8243	#[test]
8244	#[serial_test::serial]
8245	fn project_roles_personas_gated_via_resolve_and_record_chain() {
8246	    use xai_grok_test_support::EnvGuard;
8247	    let home = tempfile::tempdir().unwrap();
8248	    let _env = EnvGuard::set("GROK_HOME", home.path());
8249	    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
8250	    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
8251	    let repo = tempfile::tempdir().unwrap();
8252	    git2::Repository::init(repo.path()).unwrap();
8253	    write_project_subagent_definitions(repo.path());
8254	    run_local_for_bridge_test(|| async {
8255	        let (agent, _rx) = build_agent_with_gateway_rx();
8256	        let sid = acp::SessionId::new("roles-personas-resolve-chain");
8257	        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
8258	        handle.info.cwd = repo.path().display().to_string();
8259	        agent.insert_resident(&sid, handle);
8260	        let allowed = crate::agent::folder_trust::resolve_and_record(
8261	            repo.path(),
8262	            Some(&folder_trust_on()),
8263	            false,
8264	        );
8265	        assert!(
8266	            !allowed,
8267	            "roles/personas markers alone must resolve untrusted without a grant"
8268	        );
8269	        assert!(
8270	            !crate::agent::folder_trust::project_scope_allowed(repo.path()),
8271	            "cached verdict after resolve_and_record must stay untrusted"
8272	        );
8273	        let untrusted = agent.build_subagent_spawn_context(sid.0.as_ref());
8274	        assert!(
8275	            !untrusted.subagent_roles.contains_key("probe"),
8276	            "untrusted: project role must stay out of spawn context"
8277	        );
8278	        assert!(
8279	            !untrusted.subagent_personas.contains_key("probe"),
8280	            "untrusted: project persona must stay out of spawn context"
8281	        );
8282	        xai_grok_workspace::folder_trust::grant_folder_trust(repo.path());
8283	        let allowed = crate::agent::folder_trust::resolve_and_record(
8284	            repo.path(),
8285	            Some(&folder_trust_on()),
8286	            false,
8287	        );
8288	        assert!(allowed, "store-granted folder must resolve trusted");
8289	        let trusted = agent.build_subagent_spawn_context(sid.0.as_ref());
8290	        assert_eq!(
8291	            trusted
8292	                .subagent_roles
8293	                .get("probe")
8294	                .map(|role| role.description.as_str()),
8295	            Some("Project role")
8296	        );
8297	        assert!(
8298	            trusted.subagent_personas.contains_key("probe"),
8299	            "trusted: project persona must enter spawn context after grant"
8300	        );
8301	    });
8302	}
8303	/// Pull the next `x.ai/folder_trust/request` reverse-request off the gateway and answer it with `outcome`.
8304	/// Returns the request's decoded params.
8305	async fn answer_folder_trust_request(
8306	    gw_rx: &mut tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
8307	    outcome: &str,
8308	) -> serde_json::Value {
8309	    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), gw_rx.recv())
8310	        .await
8311	        .expect("trust request must be sent")
8312	        .expect("gateway channel open");
8313	    let xai_acp_lib::AcpClientMessage::ExtMethod(args) = msg else {
8314	        panic!("expected an ext_method reverse-request, got a different message");
8315	    };
8316	    assert_eq!(args.request.method.as_ref(), "x.ai/folder_trust/request");
8317	    let params: serde_json::Value = serde_json::from_str(args.request.params.get()).unwrap();
8318	    let resp: acp::ExtResponse = acp::ExtResponse::new(std::sync::Arc::from(
8319	        serde_json::value::to_raw_value(&serde_json::json!({ "outcome": outcome })).unwrap(),
8320	    ));
8321	    let _ = args.response_tx.send(Ok(resp));
8322	    params
8323	}
8324	#[test]
8325	#[serial_test::serial]
8326	fn interactive_trust_prompt_grant_reloads_project_mcp() {
8327	    use xai_grok_test_support::EnvGuard;
8328	    use xai_grok_workspace::trust::{TrustStore, workspace_key};
8329	    let home = tempfile::tempdir().unwrap();
8330	    let _env = EnvGuard::set("GROK_HOME", home.path());
8331	    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
8332	    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
8333	    let repo = repo_with_project_mcp_server();
8334	    let repo_path = repo.path().to_path_buf();
8335	    let remote = folder_trust_on();
8336	    run_local_for_bridge_test(|| async {
8337	        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
8338	        agent.interactive_trust_client.set(true);
8339	        crate::agent::folder_trust::resolve_and_record(&repo_path, Some(&remote), false);
8340	        assert!(
8341	            !crate::agent::folder_trust::project_scope_allowed(&repo_path),
8342	            "untrusted-with-configs workspace must gate project scope before the grant"
8343	        );
8344	        let sid = acp::SessionId::new("sess-trust");
8345	        let (mut handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
8346	        handle.info.cwd = repo_path.to_string_lossy().to_string();
8347	        agent.insert_resident(&sid, handle);
8348	        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
8349	        let params = answer_folder_trust_request(&mut gw_rx, "trust").await;
8350	        assert!(
8351	            params
8352	                .get("configKinds")
8353	                .and_then(|v| v.as_array())
8354	                .is_some_and(|k| k.iter().any(|v| v == "mcp")),
8355	            "request must summarize detected config kinds; got {params}"
8356	        );
8357	        assert_eq!(
8358	            params.get("sessionId").and_then(|v| v.as_str()),
8359	            Some("sess-trust"),
8360	            "trust request must carry the session id for leader routing; got {params}"
8361	        );
8362	        let mut saw_project_mcp = false;
8363	        let mut saw_reload_plugins = false;
8364	        let mut saw_reload_hooks = false;
8365	        for _ in 0..8 {
8366	            match tokio::time::timeout(std::time::Duration::from_secs(2), cmd_rx.recv()).await {
8367	                Ok(Some(TestSessionCommand::UpdateMcpServers { mcp_servers, .. })) => {
8368	                    saw_project_mcp |= mcp_servers
8369	                        .iter()
8370	                        .any(|s| crate::session::managed_mcp::mcp_server_name(s) == "projsrv");
8371	                }
8372	                Ok(Some(TestSessionCommand::ReloadPlugins { .. })) => {
8373	                    saw_reload_plugins = true;
8374	                }
8375	                Ok(Some(TestSessionCommand::ReloadHooks)) => saw_reload_hooks = true,
8376	                Ok(Some(_other)) => continue,
8377	                _ => break,
8378	            }
8379	            if saw_project_mcp && saw_reload_plugins && saw_reload_hooks {
8380	                break;
8381	            }
8382	        }
8383	        assert!(
8384	            saw_project_mcp,
8385	            "trust grant must reload the session's now-trusted project MCP server"
8386	        );
8387	        assert!(
8388	            saw_reload_plugins,
8389	            "trust grant must reload plugins (plugin-contributed hooks/MCP)"
8390	        );
8391	        assert!(
8392	            saw_reload_hooks,
8393	            "trust grant must reload the session's own project hooks"
8394	        );
8395	        assert!(
8396	            TrustStore::load().is_trusted(&workspace_key(&repo_path)),
8397	            "accepting the prompt must persist the trust grant"
8398	        );
8399	        assert!(
8400	            crate::agent::folder_trust::project_scope_allowed(&repo_path),
8401	            "the in-process gate must flip to trusted after the grant"
8402	        );
8403	    });
8404	}
8405	#[test]
8406	#[serial_test::serial]
8407	fn interactive_trust_prompt_reject_keeps_gated() {
8408	    use xai_grok_test_support::EnvGuard;
8409	    use xai_grok_workspace::trust::{TrustStore, workspace_key};
8410	    let home = tempfile::tempdir().unwrap();
8411	    let _env = EnvGuard::set("GROK_HOME", home.path());
8412	    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
8413	    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
8414	    let repo = repo_with_project_mcp_server();
8415	    let repo_path = repo.path().to_path_buf();
8416	    let remote = folder_trust_on();
8417	    run_local_for_bridge_test(|| async {
8418	        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
8419	        agent.interactive_trust_client.set(true);
8420	        crate::agent::folder_trust::resolve_and_record(&repo_path, Some(&remote), false);
8421	        let sid = acp::SessionId::new("sess-reject");
8422	        let (mut handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
8423	        handle.info.cwd = repo_path.to_string_lossy().to_string();
8424	        agent.insert_resident(&sid, handle);
8425	        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
8426	        let _ = answer_folder_trust_request(&mut gw_rx, "reject").await;
8427	        assert!(
8428	            tokio::time::timeout(std::time::Duration::from_millis(300), cmd_rx.recv())
8429	                .await
8430	                .is_err(),
8431	            "rejecting trust must leave the session's project servers gated (no reload)"
8432	        );
8433	        assert!(
8434	            !TrustStore::load().is_trusted(&workspace_key(&repo_path)),
8435	            "rejecting trust must leave the store unchanged"
8436	        );
8437	        assert!(
8438	            !crate::agent::folder_trust::project_scope_allowed(&repo_path),
8439	            "rejecting trust must keep the workspace gated"
8440	        );
8441	    });
8442	}
8443	#[test]
8444	#[serial_test::serial]
8445	fn interactive_trust_prompt_dormant_when_feature_off() {
8446	    use xai_grok_test_support::EnvGuard;
8447	    let home = tempfile::tempdir().unwrap();
8448	    let _env = EnvGuard::set("GROK_HOME", home.path());
8449	    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
8450	    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
8451	    let repo = repo_with_project_mcp_server();
8452	    let repo_path = repo.path().to_path_buf();
8453	    let remote = crate::util::config::RemoteSettings {
8454	        folder_trust_enabled: Some(false),
8455	        ..Default::default()
8456	    };
8457	    run_local_for_bridge_test(|| async {
8458	        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
8459	        agent.interactive_trust_client.set(true);
8460	        let sid = acp::SessionId::new("sess-dormant");
8461	        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
8462	        handle.info.cwd = repo_path.to_string_lossy().to_string();
8463	        agent.insert_resident(&sid, handle);
8464	        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
8465	        assert!(
8466	            tokio::time::timeout(std::time::Duration::from_millis(300), gw_rx.recv())
8467	                .await
8468	                .is_err(),
8469	            "feature off must emit no trust request (dormant)"
8470	        );
8471	    });
8472	}
8473	#[test]
8474	#[serial_test::serial]
8475	fn interactive_trust_prompt_no_request_without_capability() {
8476	    use xai_grok_test_support::EnvGuard;
8477	    let home = tempfile::tempdir().unwrap();
8478	    let _env = EnvGuard::set("GROK_HOME", home.path());
8479	    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
8480	    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
8481	    let repo = repo_with_project_mcp_server();
8482	    let repo_path = repo.path().to_path_buf();
8483	    let remote = folder_trust_on();
8484	    run_local_for_bridge_test(|| async {
8485	        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
8486	        assert!(!agent.interactive_trust_client.get());
8487	        let sid = acp::SessionId::new("sess-nocap");
8488	        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
8489	        handle.info.cwd = repo_path.to_string_lossy().to_string();
8490	        agent.insert_resident(&sid, handle);
8491	        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
8492	        assert!(
8493	            tokio::time::timeout(std::time::Duration::from_millis(300), gw_rx.recv())
8494	                .await
8495	                .is_err(),
8496	            "a client without the capability must get no trust request"
8497	        );
8498	    });
8499	}
8500	#[test]
8501	#[serial_test::serial]
8502	fn interactive_trust_prompt_client_error_fails_closed() {
8503	    use xai_grok_test_support::EnvGuard;
8504	    use xai_grok_workspace::trust::{TrustStore, workspace_key};
8505	    let home = tempfile::tempdir().unwrap();
8506	    let _env = EnvGuard::set("GROK_HOME", home.path());
8507	    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
8508	    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
8509	    let repo = repo_with_project_mcp_server();
8510	    let repo_path = repo.path().to_path_buf();
8511	    let remote = folder_trust_on();
8512	    run_local_for_bridge_test(|| async {
8513	        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
8514	        agent.interactive_trust_client.set(true);
8515	        crate::agent::folder_trust::resolve_and_record(&repo_path, Some(&remote), false);
8516	        let sid = acp::SessionId::new("sess-err");
8517	        let (mut handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
8518	        handle.info.cwd = repo_path.to_string_lossy().to_string();
8519	        agent.insert_resident(&sid, handle);
8520	        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
8521	        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), gw_rx.recv())
8522	            .await
8523	            .expect("trust request must be sent")
8524	            .expect("gateway channel open");
8525	        assert!(matches!(msg, xai_acp_lib::AcpClientMessage::ExtMethod(_)));
8526	        drop(msg);
8527	        assert!(
8528	            tokio::time::timeout(std::time::Duration::from_millis(300), cmd_rx.recv())
8529	                .await
8530	                .is_err(),
8531	            "a failed client round-trip must not reload the session"
8532	        );
8533	        assert!(
8534	            !TrustStore::load().is_trusted(&workspace_key(&repo_path)),
8535	            "a failed client round-trip must not grant trust"
8536	        );
8537	        assert!(!crate::agent::folder_trust::project_scope_allowed(
8538	            &repo_path
8539	        ));
8540	    });
8541	}
8542	#[test]
8543	#[serial_test::serial]
8544	fn interactive_trust_prompt_dedups_same_workspace() {
8545	    use xai_grok_test_support::EnvGuard;
8546	    let home = tempfile::tempdir().unwrap();
8547	    let _env = EnvGuard::set("GROK_HOME", home.path());
8548	    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
8549	    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
8550	    let repo = repo_with_project_mcp_server();
8551	    let repo_path = repo.path().to_path_buf();
8552	    let remote = folder_trust_on();
8553	    run_local_for_bridge_test(|| async {
8554	        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
8555	        agent.interactive_trust_client.set(true);
8556	        crate::agent::folder_trust::resolve_and_record(&repo_path, Some(&remote), false);
8557	        let sid = acp::SessionId::new("sess-dedup");
8558	        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
8559	        handle.info.cwd = repo_path.to_string_lossy().to_string();
8560	        agent.insert_resident(&sid, handle);
8561	        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
8562	        let first = tokio::time::timeout(std::time::Duration::from_secs(2), gw_rx.recv()).await;
8563	        assert!(
8564	            matches!(first, Ok(Some(xai_acp_lib::AcpClientMessage::ExtMethod(_)))),
8565	            "first prompt for an untrusted workspace must emit a request"
8566	        );
8567	        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
8568	        assert!(
8569	            tokio::time::timeout(std::time::Duration::from_millis(300), gw_rx.recv())
8570	                .await
8571	                .is_err(),
8572	            "a workspace already prompted this process must not be re-prompted"
8573	        );
8574	    });
8575	}
8576	/// Which reload commands a session received after a grant.
8577	struct ReloadCmds {
8578	    update_mcp: bool,
8579	    reload_plugins: bool,
8580	    reload_hooks: bool,
8581	    mcp_names: Vec<String>,
8582	}
8583	/// Drain a session's command channel for the post-grant reload trio (`UpdateMcpServers`, `ReloadPlugins`, `ReloadHooks`).
8584	/// Captures the merged MCP server names so a test can assert per-cwd reload.
8585	async fn drain_reload_commands(
8586	    cmd_rx: &mut tokio::sync::mpsc::UnboundedReceiver<TestSessionCommand>,
8587	) -> ReloadCmds {
8588	    let mut out = ReloadCmds {
8589	        update_mcp: false,
8590	        reload_plugins: false,
8591	        reload_hooks: false,
8592	        mcp_names: Vec::new(),
8593	    };
8594	    for _ in 0..8 {
8595	        match tokio::time::timeout(std::time::Duration::from_secs(2), cmd_rx.recv()).await {
8596	            Ok(Some(TestSessionCommand::UpdateMcpServers { mcp_servers, .. })) => {
8597	                out.update_mcp = true;
8598	                out.mcp_names = mcp_servers
8599	                    .iter()
8600	                    .map(|s| crate::session::managed_mcp::mcp_server_name(s).to_string())
8601	                    .collect();
8602	            }
8603	            Ok(Some(TestSessionCommand::ReloadPlugins { .. })) => {
8604	                out.reload_plugins = true;
8605	            }
8606	            Ok(Some(TestSessionCommand::ReloadHooks)) => out.reload_hooks = true,
8607	            Ok(Some(_other)) => continue,
8608	            _ => break,
8609	        }
8610	        if out.update_mcp && out.reload_plugins && out.reload_hooks {
8611	            break;
8612	        }
8613	    }
8614	    out
8615	}
8616	#[test]
8617	#[serial_test::serial]
8618	fn interactive_trust_prompt_reloads_all_same_workspace_sessions() {
8619	    use xai_grok_test_support::EnvGuard;
8620	    let home = tempfile::tempdir().unwrap();
8621	    let _env = EnvGuard::set("GROK_HOME", home.path());
8622	    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
8623	    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
8624	    let repo = repo_with_project_mcp_server();
8625	    let root = repo.path().to_path_buf();
8626	    let subdir = root.join("sub");
8627	    std::fs::create_dir_all(&subdir).unwrap();
8628	    std::fs::write(
8629	        subdir.join(".mcp.json"),
8630	        r#"{"mcpServers":{"subsrv":{"command":"echo","args":["hi"]}}}"#,
8631	    )
8632	    .unwrap();
8633	    let other = repo_with_project_mcp_server();
8634	    let other_path = other.path().to_path_buf();
8635	    let remote = folder_trust_on();
8636	    run_local_for_bridge_test(|| async {
8637	        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
8638	        agent.interactive_trust_client.set(true);
8639	        crate::agent::folder_trust::resolve_and_record(&root, Some(&remote), false);
8640	        let sid_root = acp::SessionId::new("sess-root");
8641	        let (mut h_root, _t1, mut rx_root) = make_live_session_handle(&sid_root, None);
8642	        h_root.info.cwd = root.to_string_lossy().to_string();
8643	        agent.insert_resident(&sid_root, h_root);
8644	        let sid_sub = acp::SessionId::new("sess-sub");
8645	        let (mut h_sub, _t2, mut rx_sub) = make_live_session_handle(&sid_sub, None);
8646	        h_sub.info.cwd = subdir.to_string_lossy().to_string();
8647	        agent.insert_resident(&sid_sub, h_sub);
8648	        let sid_other = acp::SessionId::new("sess-other");
8649	        let (mut h_other, _t3, mut rx_other) = make_live_session_handle(&sid_other, None);
8650	        h_other.info.cwd = other_path.to_string_lossy().to_string();
8651	        agent.insert_resident(&sid_other, h_other);
8652	        agent.maybe_spawn_interactive_trust_prompt(&sid_root, &root, Some(&remote));
8653	        let _ = answer_folder_trust_request(&mut gw_rx, "trust").await;
8654	        let root_cmds = drain_reload_commands(&mut rx_root).await;
8655	        assert!(
8656	            root_cmds.update_mcp && root_cmds.reload_plugins && root_cmds.reload_hooks,
8657	            "root session must get UpdateMcpServers + ReloadPlugins + ReloadHooks"
8658	        );
8659	        let sub_cmds = drain_reload_commands(&mut rx_sub).await;
8660	        assert!(
8661	            sub_cmds.update_mcp && sub_cmds.reload_plugins && sub_cmds.reload_hooks,
8662	            "subdir session must get UpdateMcpServers + ReloadPlugins + ReloadHooks"
8663	        );
8664	        assert!(
8665	            sub_cmds.mcp_names.iter().any(|n| n == "subsrv"),
8666	            "subdir session must reload against its own cwd (expect subsrv); got {:?}",
8667	            sub_cmds.mcp_names
8668	        );
8669	        assert!(
8670	            tokio::time::timeout(std::time::Duration::from_millis(300), rx_other.recv())
8671	                .await
8672	                .is_err(),
8673	            "a session under a different workspace_key must not be reloaded"
8674	        );
8675	    });
8676	}
8677	#[test]
8678	#[serial_test::serial]
8679	fn interactive_trust_prompt_reprompts_after_untrust() {
8680	    use xai_grok_test_support::EnvGuard;
8681	    use xai_hooks_plugins_types::HooksAction;
8682	    let home = tempfile::tempdir().unwrap();
8683	    let _env = EnvGuard::set("GROK_HOME", home.path());
8684	    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
8685	    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
8686	    let repo = repo_with_project_mcp_server();
8687	    let repo_path = repo.path().to_path_buf();
8688	    let remote = folder_trust_on();
8689	    run_local_for_bridge_test(|| async {
8690	        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
8691	        agent.interactive_trust_client.set(true);
8692	        crate::agent::folder_trust::resolve_and_record(&repo_path, Some(&remote), false);
8693	        let sid = acp::SessionId::new("sess-reprompt");
8694	        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
8695	        handle.info.cwd = repo_path.to_string_lossy().to_string();
8696	        agent.insert_resident(&sid, handle);
8697	        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
8698	        assert!(
8699	            matches!(
8700	                tokio::time::timeout(std::time::Duration::from_secs(2), gw_rx.recv()).await,
8701	                Ok(Some(xai_acp_lib::AcpClientMessage::ExtMethod(_)))
8702	            ),
8703	            "first prompt must emit a request"
8704	        );
8705	        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
8706	        assert!(
8707	            tokio::time::timeout(std::time::Duration::from_millis(200), gw_rx.recv())
8708	                .await
8709	                .is_err(),
8710	            "a prompted workspace must be suppressed before untrust"
8711	        );
8712	        let _ = tokio::time::timeout(
8713	            std::time::Duration::from_millis(200),
8714	            agent.execute_hooks_action(&sid, HooksAction::Untrust),
8715	        )
8716	        .await;
8717	        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
8718	        assert!(
8719	            matches!(
8720	                tokio::time::timeout(std::time::Duration::from_secs(2), gw_rx.recv()).await,
8721	                Ok(Some(xai_acp_lib::AcpClientMessage::ExtMethod(_)))
8722	            ),
8723	            "after untrust clears the dedup, the workspace must be promptable again"
8724	        );
8725	    });
8726	}
8727	fn ann(id: &str) -> xai_grok_announcements::RemoteAnnouncement {
8728	    xai_grok_announcements::RemoteAnnouncement {
8729	        id: Some(id.to_string()),
8730	        message: Some(format!("{id}-msg")),
8731	        severity: Some("critical".to_string()),
8732	        ..Default::default()
8733	    }
8734	}
8735	/// `RemoteSettings` with only `announcements` set (callers add sentinel fields as needed).
8736	fn settings_with(
8737	    announcements: Option<Vec<xai_grok_announcements::RemoteAnnouncement>>,
8738	) -> crate::util::config::RemoteSettings {
8739	    crate::util::config::RemoteSettings {
8740	        announcements,
8741	        ..Default::default()
8742	    }
8743	}
8744	fn test_now() -> chrono::DateTime<chrono::Utc> {
8745	    chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
8746	        .unwrap()
8747	        .with_timezone(&chrono::Utc)
8748	}
8749	/// Pushes must carry strictly increasing generations, seeded from unix-epoch seconds.
8750	/// A restarted leader then still beats pager watermarks that survived re-election (`AppView.announcements_last_gen` is never reset).
8751	#[tokio::test]
8752	async fn announcements_gen_seeds_from_epoch_and_strictly_increases() {
8753	    let agent = build_minimal_agent_for_tests();
8754	    let epoch_before = std::time::SystemTime::now()
8755	        .duration_since(std::time::UNIX_EPOCH)
8756	        .unwrap()
8757	        .as_secs();
8758	    let first = agent.next_announcements_gen();
8759	    let second = agent.next_announcements_gen();
8760	    assert!(
8761	        first >= epoch_before,
8762	        "first gen must be epoch-seeded: {first} < {epoch_before}"
8763	    );
8764	    assert!(
8765	        second > first,
8766	        "gens must strictly increase: {first} -> {second}"
8767	    );
8768	    let far_ahead = first + 1_000_000;
8769	    agent.announcements_gen.set(far_ahead);
8770	    assert_eq!(agent.next_announcements_gen(), far_ahead + 1);
8771	}
8772	/// An unchanged visible list must not produce a push (idle steady-state is silent); a changed one, including clearing to empty, must.
8773	#[test]
8774	fn announcements_push_gate_emits_only_on_change() {
8775	    let now = test_now();
8776	    assert_eq!(
8777	        announcements_push_payload(None, &[], now, AnnouncementsPushMode::IfChanged),
8778	        None
8779	    );
8780	    let list_a = vec![ann("a")];
8781	    assert_eq!(
8782	        announcements_push_payload(
8783	            Some(list_a.as_slice()),
8784	            &[],
8785	            now,
8786	            AnnouncementsPushMode::IfChanged
8787	        ),
8788	        Some(list_a.clone())
8789	    );
8790	    assert_eq!(
8791	        announcements_push_payload(
8792	            Some(list_a.as_slice()),
8793	            &list_a,
8794	            now,
8795	            AnnouncementsPushMode::IfChanged
8796	        ),
8797	        None
8798	    );
8799	    let list_ab = vec![ann("a"), ann("b")];
8800	    assert_eq!(
8801	        announcements_push_payload(
8802	            Some(list_ab.as_slice()),
8803	            &list_a,
8804	            now,
8805	            AnnouncementsPushMode::IfChanged
8806	        ),
8807	        Some(list_ab.clone())
8808	    );
8809	    assert_eq!(
8810	        announcements_push_payload(None, &list_ab, now, AnnouncementsPushMode::IfChanged),
8811	        Some(vec![])
8812	    );
8813	}
8814	/// `seed` (per-client initialize) re-emits an unchanged non-empty list for the freshly attached client.
8815	/// It stays silent when there is nothing to show.
8816	#[test]
8817	fn announcements_push_gate_seed_reemits_nonempty_only() {
8818	    let now = test_now();
8819	    let list_a = vec![ann("a")];
8820	    assert_eq!(
8821	        announcements_push_payload(
8822	            Some(list_a.as_slice()),
8823	            &list_a,
8824	            now,
8825	            AnnouncementsPushMode::SeedNewClient
8826	        ),
8827	        Some(list_a.clone()),
8828	        "seed must re-push an unchanged non-empty list"
8829	    );
8830	    assert_eq!(
8831	        announcements_push_payload(None, &[], now, AnnouncementsPushMode::SeedNewClient),
8832	        None,
8833	        "seed with nothing visible must stay silent"
8834	    );
8835	}
8836	/// `/new` forces a push even when the visible list is unchanged, including unchanged-empty.
8837	/// The pager then re-merges its config-layer (requirements/user/managed TOML) announcements from local mid-session edits.
8838	#[test]
8839	fn announcements_push_gate_force_mode_pushes_unchanged_and_empty() {
8840	    let now = test_now();
8841	    let list_a = vec![ann("a")];
8842	    assert_eq!(
8843	        announcements_push_payload(
8844	            Some(list_a.as_slice()),
8845	            &list_a,
8846	            now,
8847	            AnnouncementsPushMode::Force
8848	        ),
8849	        Some(list_a.clone()),
8850	        "force must push an unchanged list"
8851	    );
8852	    assert_eq!(
8853	        announcements_push_payload(None, &[], now, AnnouncementsPushMode::Force),
8854	        Some(vec![]),
8855	        "force must push even an unchanged empty list"
8856	    );
8857	}
8858	/// An addition that is already expired on arrival never becomes visible, so it must not re-emit.
8859	#[test]
8860	fn announcements_push_gate_ignores_expired_only_addition() {
8861	    let now = test_now();
8862	    let expired = xai_grok_announcements::RemoteAnnouncement {
8863	        expires_at: Some("2000-01-01T00:00:00Z".to_string()),
8864	        ..ann("expired")
8865	    };
8866	    let list_a = vec![ann("a")];
8867	    let stored = vec![ann("a"), expired];
8868	    assert_eq!(
8869	        announcements_push_payload(
8870	            Some(stored.as_slice()),
8871	            &list_a,
8872	            now,
8873	            AnnouncementsPushMode::IfChanged
8874	        ),
8875	        None,
8876	        "an already-expired addition must not re-emit"
8877	    );
8878	}
8879	/// A previously emitted item that passes its `expires_at` between gate runs must emit the shrunken (here: empty) list exactly once.
8880	/// Live banners then clear on time instead of outliving their own expiry.
8881	#[test]
8882	fn announcements_push_gate_emits_on_expiry_crossing() {
8883	    let expiring = xai_grok_announcements::RemoteAnnouncement {
8884	        expires_at: Some("2026-06-01T00:00:00Z".to_string()),
8885	        ..ann("soon")
8886	    };
8887	    let stored = vec![expiring.clone()];
8888	    let before = chrono::DateTime::parse_from_rfc3339("2026-05-31T23:59:00Z")
8889	        .unwrap()
8890	        .with_timezone(&chrono::Utc);
8891	    let emitted = announcements_push_payload(
8892	        Some(stored.as_slice()),
8893	        &[],
8894	        before,
8895	        AnnouncementsPushMode::IfChanged,
8896	    )
8897	    .expect("live item must emit");
8898	    assert_eq!(emitted, stored);
8899	    let after = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:01:00Z")
8900	        .unwrap()
8901	        .with_timezone(&chrono::Utc);
8902	    assert_eq!(
8903	        announcements_push_payload(
8904	            Some(stored.as_slice()),
8905	            &emitted,
8906	            after,
8907	            AnnouncementsPushMode::IfChanged
8908	        ),
8909	        Some(vec![]),
8910	        "expiry crossing must emit the shrunken list"
8911	    );
8912	    assert_eq!(
8913	        announcements_push_payload(
8914	            Some(stored.as_slice()),
8915	            &[],
8916	            after,
8917	            AnnouncementsPushMode::IfChanged
8918	        ),
8919	        None
8920	    );
8921	}
8922	/// A poll apply must touch ONLY `remote_settings.announcements` (and the request-encoding advertisement).
8923	/// Every other stored field keeps its pre-poll value (full reapply stays owned by startup, auth, and `/new`).
8924	#[tokio::test]
8925	#[serial_test::serial(remote_sig_disarm)]
8926	async fn polled_settings_apply_touches_announcements_only() {
8927	    let agent = build_minimal_agent_for_tests();
8928	    let mut stored = settings_with(Some(vec![ann("old")]));
8929	    stored.tips = Some(vec!["stored-tip".to_string()]);
8930	    stored.allow_access = Some(true);
8931	    stored.default_model = Some("stored-model".to_string());
8932	    agent.cfg.borrow_mut().remote_settings = Some(stored);
8933	    let mut fresh = settings_with(Some(vec![ann("new")]));
8934	    fresh.tips = Some(vec!["fresh-tip".to_string()]);
8935	    fresh.allow_access = Some(false);
8936	    fresh.default_model = Some("fresh-model".to_string());
8937	    agent.apply_polled_settings(fresh, Some((Some(vec![ann("old")]), vec![])));
8938	    let cfg = agent.cfg.borrow();
8939	    let after = cfg
8940	        .remote_settings
8941	        .as_ref()
8942	        .expect("settings still present");
8943	    assert_eq!(after.announcements, Some(vec![ann("new")]));
8944	    assert_eq!(
8945	        after.tips,
8946	        Some(vec!["stored-tip".to_string()]),
8947	        "tips must be untouched by a poll apply"
8948	    );
8949	    assert_eq!(
8950	        after.allow_access,
8951	        Some(true),
8952	        "allow_access must be untouched by a poll apply"
8953	    );
8954	    assert_eq!(
8955	        after.default_model.as_deref(),
8956	        Some("stored-model"),
8957	        "default_model must be untouched by a poll apply"
8958	    );
8959	}
8960	/// The request-encoding advertisement is read per turn, so a poll must carry it:
8961	/// turning the proxy flag off has to stop compression before the next `/new` or restart.
8962	#[tokio::test]
8963	#[serial_test::serial]
8964	#[serial_test::serial(remote_sig_disarm)]
8965	async fn polled_settings_apply_refreshes_accept_request_encodings() {
8966	    use xai_grok_config_types::RemoteRequestEncoding;
8967	    use xai_grok_sampler::RequestCompression;
8968	    let _env = crate::env::EnvVarGuard::remove("GROK_REQUEST_COMPRESSION");
8969	    let agent = build_minimal_agent_for_tests();
8970	    let proxy = agent.cfg.borrow().endpoints.proxy_url();
8971	    let mut stored = settings_with(Some(vec![ann("old")]));
8972	    stored.accept_request_encodings = vec![RemoteRequestEncoding::Zstd];
8973	    crate::util::config::cache_remote_accept_request_encodings(
8974	        &proxy,
8975	        &stored.accept_request_encodings,
8976	    );
8977	    agent.cfg.borrow_mut().remote_settings = Some(stored);
8978	    assert_eq!(
8979	        crate::util::config::request_compression_for_url(&proxy),
8980	        RequestCompression::Zstd
8981	    );
8982	    agent.apply_polled_settings(
8983	        settings_with(Some(vec![ann("old")])),
8984	        Some((Some(vec![ann("old")]), vec![RemoteRequestEncoding::Zstd])),
8985	    );
8986	    assert_eq!(
8987	        crate::util::config::request_compression_for_url(&proxy),
8988	        RequestCompression::None,
8989	        "a poll that no longer advertises zstd must disarm compression"
8990	    );
8991	    assert!(
8992	        agent
8993	            .cfg
8994	            .borrow()
8995	            .remote_settings
8996	            .as_ref()
8997	            .is_some_and(|s| s.accept_request_encodings.is_empty()),
8998	        "the stored copy must match so a later re-apply cannot re-arm it"
8999	    );
9000	}
9001	/// `--cli-chat-proxy-base-url` points `cfg.endpoints` away from the disk config and the
9002	/// post-auth fetch reads `/v1/settings` from `cfg.endpoints`: the advertisement must be
9003	/// keyed under that origin, the one the poll and the model routes also use.
9004	#[tokio::test]
9005	#[serial_test::serial]
9006	#[serial_test::serial(remote_sig_disarm)]
9007	async fn settings_apply_keys_the_advertisement_under_the_configured_proxy() {
9008	    use xai_grok_config_types::RemoteRequestEncoding;
9009	    use xai_grok_sampler::RequestCompression;
9010	    let _env = crate::env::EnvVarGuard::remove("GROK_REQUEST_COMPRESSION");
9011	    let agent = build_minimal_agent_for_tests();
9012	    let flag_proxy = "http://localhost:20016/v1";
9013	    {
9014	        let mut cfg = agent.cfg.borrow_mut();
9015	        cfg.endpoints.cli_chat_proxy_base_url = Some(flag_proxy.to_owned());
9016	        cfg.remote_settings = Some(crate::util::config::RemoteSettings {
9017	            accept_request_encodings: vec![RemoteRequestEncoding::Zstd],
9018	            ..Default::default()
9019	        });
9020	    }
9021	    agent.on_remote_settings_changed();
9022	    assert_eq!(
9023	        crate::util::config::request_compression_for_url(flag_proxy),
9024	        RequestCompression::Zstd,
9025	        "the proxy that served the settings must be the one compressed toward"
9026	    );
9027	    let prod = crate::env::PROD_CLI_CHAT_PROXY_BASE_URL;
9028	    assert_eq!(
9029	        crate::util::config::request_compression_for_url(prod),
9030	        RequestCompression::None,
9031	        "{prod} did not serve these settings"
9032	    );
9033	    crate::util::config::cache_remote_accept_request_encodings(flag_proxy, &[]);
9034	}
9035	/// A poll response that straddles a full reapply carries an older server view: when the
9036	/// reapply withdrew the advertisement mid-fetch, the poll must skip rather than re-arm it.
9037	#[tokio::test]
9038	#[serial_test::serial]
9039	#[serial_test::serial(remote_sig_disarm)]
9040	async fn polled_settings_apply_skips_when_the_advertisement_changed_mid_fetch() {
9041	    use xai_grok_config_types::RemoteRequestEncoding;
9042	    use xai_grok_sampler::RequestCompression;
9043	    let _env = crate::env::EnvVarGuard::remove("GROK_REQUEST_COMPRESSION");
9044	    let agent = build_minimal_agent_for_tests();
9045	    let proxy = agent.cfg.borrow().endpoints.proxy_url();
9046	    let mut advertised = settings_with(Some(vec![ann("old")]));
9047	    advertised.accept_request_encodings = vec![RemoteRequestEncoding::Zstd];
9048	    let pre_fetch = Some((Some(vec![ann("old")]), vec![RemoteRequestEncoding::Zstd]));
9049	    agent.cfg.borrow_mut().remote_settings = Some(settings_with(Some(vec![ann("old")])));
9050	    crate::util::config::cache_remote_accept_request_encodings(&proxy, &[]);
9051	    agent.apply_polled_settings(advertised, pre_fetch);
9052	    assert_eq!(
9053	        crate::util::config::request_compression_for_url(&proxy),
9054	        RequestCompression::None,
9055	        "a stale poll must not re-arm an advertisement a full reapply withdrew"
9056	    );
9057	    assert!(
9058	        agent
9059	            .cfg
9060	            .borrow()
9061	            .remote_settings
9062	            .as_ref()
9063	            .is_some_and(|s| s.accept_request_encodings.is_empty()),
9064	        "the mid-fetch writer's store must win over the stale poll result"
9065	    );
9066	}
9067	/// A poll apply must never fabricate `remote_settings` from scratch.
9068	/// The full-refresh owners key their retry and gating on `is_none()`, so absence must stay observable.
9069	#[tokio::test]
9070	async fn polled_settings_apply_never_fabricates_settings() {
9071	    let agent = build_minimal_agent_for_tests();
9072	    agent.cfg.borrow_mut().remote_settings = None;
9073	    agent.apply_polled_settings(settings_with(Some(vec![ann("a")])), None);
9074	    assert!(
9075	        agent.cfg.borrow().remote_settings.is_none(),
9076	        "a poll must leave absent remote_settings absent"
9077	    );
9078	}
9079	/// A full-refresh writer landing during the poll's fetch makes the poll's result stale.
9080	/// The apply must skip rather than clobber the fresher store (the next tick reconciles).
9081	#[tokio::test]
9082	async fn polled_settings_apply_skips_when_writer_landed_mid_fetch() {
9083	    let agent = build_minimal_agent_for_tests();
9084	    let pre_fetch = Some((Some(vec![ann("old")]), vec![]));
9085	    agent.cfg.borrow_mut().remote_settings = Some(settings_with(Some(vec![ann("mid-fetch")])));
9086	    agent.apply_polled_settings(settings_with(Some(vec![ann("stale-poll")])), pre_fetch);
9087	    assert_eq!(
9088	        agent
9089	            .cfg
9090	            .borrow()
9091	            .remote_settings
9092	            .as_ref()
9093	            .and_then(|s| s.announcements.clone()),
9094	        Some(vec![ann("mid-fetch")]),
9095	        "the mid-fetch writer's store must win over the stale poll result"
9096	    );
9097	}
9098	/// End-to-end through the shared gate: every emission advances the baseline and carries a strictly larger gen.
9099	/// Unchanged state is silent unless seeding a new client.
9100	#[tokio::test]
9101	async fn emit_announcements_gate_emits_updates_baseline_and_bumps_gen() {
9102	    let (agent, mut rx) = build_agent_with_gateway_rx();
9103	    agent.cfg.borrow_mut().remote_settings = Some(settings_with(Some(vec![ann("a")])));
9104	    let recv_gen =
9105	        |rx: &mut tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>| {
9106	            let msg = rx.try_recv().expect("expected an announcements push");
9107	            let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg else {
9108	                panic!("expected ExtNotification, got another message kind");
9109	            };
9110	            assert_eq!(args.request.method.as_ref(), "x.ai/announcements/update");
9111	            let parsed: serde_json::Value =
9112	                serde_json::from_str(args.request.params.get()).expect("valid JSON payload");
9113	            parsed
9114	                .get("gen")
9115	                .and_then(|g| g.as_u64())
9116	                .expect("gen field")
9117	        };
9118	    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
9119	    let first_gen = recv_gen(&mut rx);
9120	    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
9121	    assert!(rx.try_recv().is_err(), "unchanged list must not re-push");
9122	    agent.emit_announcements(AnnouncementsPushMode::SeedNewClient);
9123	    let seed_gen = recv_gen(&mut rx);
9124	    assert!(
9125	        seed_gen > first_gen,
9126	        "gen must strictly increase: {first_gen} -> {seed_gen}"
9127	    );
9128	    agent.cfg.borrow_mut().remote_settings = Some(settings_with(None));
9129	    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
9130	    let clear_gen = recv_gen(&mut rx);
9131	    assert!(clear_gen > seed_gen);
9132	    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
9133	    assert!(
9134	        rx.try_recv().is_err(),
9135	        "cleared state must push exactly once"
9136	    );
9137	    agent.emit_announcements(AnnouncementsPushMode::Force);
9138	    let force_gen = recv_gen(&mut rx);
9139	    assert!(
9140	        force_gen > clear_gen,
9141	        "forced push must keep gens increasing"
9142	    );
9143	}
9144	/// A send the gateway channel rejects must not advance the last-emitted baseline.
9145	/// The next gate call then re-diffs and re-pushes the same list (the poll's natural retry, no dedicated retry machinery).
9146	#[tokio::test]
9147	async fn emit_announcements_gate_keeps_baseline_on_failed_send_and_retries() {
9148	    let (mut agent, rx) = build_agent_with_gateway_rx();
9149	    agent.cfg.borrow_mut().remote_settings = Some(settings_with(Some(vec![ann("a")])));
9150	    drop(rx);
9151	    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
9152	    assert!(
9153	        agent.last_emitted_announcements.borrow().is_empty(),
9154	        "a failed send must leave the baseline untouched"
9155	    );
9156	    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
9157	    agent.gateway = GatewaySender::new(tx);
9158	    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
9159	    let msg = rx
9160	        .try_recv()
9161	        .expect("next gate call must re-push after a failed send");
9162	    let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg else {
9163	        panic!("expected ExtNotification, got another message kind");
9164	    };
9165	    assert_eq!(args.request.method.as_ref(), "x.ai/announcements/update");
9166	    assert_eq!(
9167	        *agent.last_emitted_announcements.borrow(),
9168	        vec![ann("a")],
9169	        "a successful send advances the baseline"
9170	    );
9171	    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
9172	    assert!(rx.try_recv().is_err(), "unchanged list must not re-push");
9173	}
9174	mod direct_hub_cloud_removed {
9175	    use super::super::{DIRECT_HUB_CLOUD_REMOVED_MSG, reject_direct_hub_cloud_meta};
9176	    use crate::agent::config::HubConfig;
9177	    fn assert_direct_hub_error(err: agent_client_protocol::Error) {
9178	        assert_eq!(
9179	            err.data.as_ref(),
9180	            Some(&serde_json::Value::String(
9181	                DIRECT_HUB_CLOUD_REMOVED_MSG.to_string()
9182	            )),
9183	            "error data must be the exact D8 message, got: {err:?}"
9184	        );
9185	        assert_eq!(
9186	            err.code,
9187	            agent_client_protocol::ErrorCode::InvalidParams,
9188	            "must be invalid_params, got: {err:?}"
9189	        );
9190	    }
9191	    #[test]
9192	    fn cloud_server_id_meta_is_hard_error() {
9193	        let meta = serde_json::json!({ "x.ai/cloud_server_id": "srv-123" });
9194	        let err = reject_direct_hub_cloud_meta(meta.as_object()).expect_err("must reject");
9195	        assert_direct_hub_error(err);
9196	    }
9197	    #[test]
9198	    fn cloud_server_id_null_still_present_is_hard_error() {
9199	        let meta = serde_json::json!({ "x.ai/cloud_server_id": null });
9200	        let err = reject_direct_hub_cloud_meta(meta.as_object()).expect_err("must reject");
9201	        assert_direct_hub_error(err);
9202	    }
9203	    #[test]
9204	    fn cloud_server_id_with_gateway_meta_still_hard_error() {
9205	        let meta = serde_json::json!({
9206	            "x.ai/cloud_server_id": "srv-legacy",
9207	            "envId": "env-1",
9208	            "x.ai/cloud_existing_workspace": {
9209	                "server_id": "ws-1",
9210	                "cwd": "/workspace"
9211	            }
9212	        });
9213	        let err = reject_direct_hub_cloud_meta(meta.as_object()).expect_err("Direct stamp wins");
9214	        assert_direct_hub_error(err);
9215	    }
9216	    #[test]
9217	    fn absent_or_empty_meta_ok() {
9218	        assert!(reject_direct_hub_cloud_meta(None).is_ok());
9219	        assert!(reject_direct_hub_cloud_meta(serde_json::json!({}).as_object()).is_ok());
9220	        assert!(
9221	            reject_direct_hub_cloud_meta(
9222	                serde_json::json!({
9223	                    "envId": "env-1"
9224	                })
9225	                .as_object()
9226	            )
9227	            .is_ok()
9228	        );
9229	        assert!(
9230	            reject_direct_hub_cloud_meta(
9231	                serde_json::json!({
9232	                    "x.ai/cloud_existing_workspace": {
9233	                        "server_id": "ws-1",
9234	                        "cwd": "/workspace"
9235	                    }
9236	                })
9237	                .as_object()
9238	            )
9239	            .is_ok()
9240	        );
9241	    }
9242	    #[test]
9243	    fn hub_url_gating_matrix() {
9244	        let with_url = HubConfig {
9245	            url: Some("wss://hub.example/ws".into()),
9246	        };
9247	        let without_url = HubConfig { url: None };
9248	        let blank = HubConfig {
9249	            url: Some("   ".into()),
9250	        };
9251	        assert!(with_url.is_enabled());
9252	        assert!(!without_url.is_enabled());
9253	        assert!(!blank.is_enabled());
9254	    }
9255	    #[test]
9256	    fn hub_config_is_url_only_workspace_default() {
9257	        let json = serde_json::to_value(HubConfig {
9258	            url: Some("wss://hub.example/ws".into()),
9259	        })
9260	        .expect("serialize");
9261	        let obj = json.as_object().expect("object");
9262	        assert_eq!(
9263	            obj.keys().collect::<Vec<_>>(),
9264	            vec!["url"],
9265	            "HubConfig must only serialize url (no proxy-mode fields)"
9266	        );
9267	        let from_legacy: HubConfig = serde_json::from_value(serde_json::json!({
9268	            "url": "wss://hub.example/ws",
9269	            "workspace_mode": "remote",
9270	            "send_turn_hooks": false,
9271	        }))
9272	        .expect("ignore unknown fields");
9273	        assert_eq!(from_legacy.url.as_deref(), Some("wss://hub.example/ws"));
9274	    }
9275	}
9276	mod soft_default_settings_emit {
9277	    use super::*;
9278	    #[tokio::test]
9279	    async fn emit_settings_update_carries_permission_mode_from_cfg() {
9280	        use crate::agent::config::Config as AgentConfig;
9281	        use xai_grok_login::{AuthManager, GrokComConfig};
9282	        let local = tokio::task::LocalSet::new();
9283	        local
9284	            .run_until(async {
9285	                let temp_dir = tempfile::tempdir().unwrap();
9286	                let auth_manager = std::sync::Arc::new(AuthManager::new(
9287	                    temp_dir.path(),
9288	                    GrokComConfig::default(),
9289	                ));
9290	                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
9291	                let gateway = GatewaySender::new(tx);
9292	                let cfg = AgentConfig {
9293	                    remote_settings: Some(crate::util::config::RemoteSettings {
9294	                        permission_mode: Some("always-approve".into()),
9295	                        slash_command_tags: Some(
9296	                            [("workflows".to_string(), "new".to_string())]
9297	                                .into_iter()
9298	                                .collect(),
9299	                        ),
9300	                        ..Default::default()
9301	                    }),
9302	                    ..Default::default()
9303	                };
9304	                let agent = MvpAgent::new(gateway, &cfg, auth_manager, None, None)
9305	                    .expect("valid test config");
9306	                agent.cfg.borrow_mut().remote_settings = cfg.remote_settings.clone();
9307	                agent.emit_settings_update_notification();
9308	                let msg = rx.try_recv().expect("settings/update must be emitted");
9309	                let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg else {
9310	                    panic!("expected ExtNotification, got {msg:?}");
9311	                };
9312	                assert_eq!(args.request.method.as_ref(), "x.ai/settings/update");
9313	                let params: serde_json::Value =
9314	                    serde_json::from_str(args.request.params.get()).expect("parse params");
9315	                assert_eq!(
9316	                    params.get("permission_mode").and_then(|v| v.as_str()),
9317	                    Some("always-approve"),
9318	                    "post-auth emit must carry remote permission_mode for first session"
9319	                );
9320	                assert_eq!(
9321	                    params
9322	                        .get("slash_command_tags")
9323	                        .and_then(|v| v.get("workflows"))
9324	                        .and_then(|v| v.as_str()),
9325	                    Some("new"),
9326	                    "post-auth emit must carry remote slash_command_tags"
9327	                );
9328	                let _ = args.response_tx.send(Ok(()));
9329	            })
9330	            .await;
9331	    }
9332	}
9333	#[test]
9334	fn subagent_rate_limit_max_attempts_resolution_precedence() {
9335	    for (config_toml, remote, env, expected) in [
9336	        (Some(3), Some(5), Some(7), 7),
9337	        (Some(3), Some(5), None, 3),
9338	        (None, Some(5), None, 5),
9339	        (None, None, None, 8),
9340	        (None, None, Some(100), 32),
9341	        (Some(50), None, None, 32),
9342	        (None, Some(99), None, 32),
9343	    ] {
9344	        assert_eq!(
9345	            resolve_subagent_rate_limit_max_attempts(config_toml, remote, env),
9346	            expected,
9347	            "config={config_toml:?} remote={remote:?} env={env:?}"
9348	        );
9349	    }
9350	}
9351	#[test]
9352	fn subagent_rate_limit_max_attempts_env_is_parsed_leniently() {
9353	    for (input, expected) in [
9354	        (None, None),
9355	        (Some(""), None),
9356	        (Some("   "), None),
9357	        (Some("abc"), None),
9358	        (Some("-1"), None),
9359	        (Some("99999999999"), None),
9360	        (Some(" 5 "), Some(5)),
9361	    ] {
9362	        assert_eq!(
9363	            parse_subagent_rate_limit_max_attempts(input),
9364	            expected,
9365	            "input={input:?}"
9366	        );
9367	    }
9368	}
9369	#[cfg(feature = "dhat-heap")]
9370	mod dhat_soak;
9371	/// A leader multiplexes many clients behind one `initialize`, so the answer has to travel with the session.
9372	/// Without the session-meta read, one terminal with the row off decides for every other terminal sharing the leader.
9373	/// Silence means off, since the payload costs a git discovery and three round trips.
9374	#[test]
9375	fn session_meta_outranks_the_client_that_started_the_process() {
9376	    let says_nothing = || {
9377	        acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
9378	            acp::ClientCapabilities::new()
9379	                .fs(acp::FileSystemCapabilities::new())
9380	                .terminal(false),
9381	        )
9382	    };
9383	    let wants_a_row = |meta: Option<acp::Meta>, init: acp::InitializeRequest| {
9384	        MvpAgent::resolve_status_line_capability(meta.as_ref(), &init)
9385	    };
9386	    let on = init_advertising_status_line(true);
9387	    let off = init_advertising_status_line(false);
9388	    assert!(
9389	        wants_a_row(Some(status_line_meta(true)), off),
9390	        "a leader that wants a row outranks the client that started the process"
9391	    );
9392	    assert!(
9393	        !wants_a_row(Some(status_line_meta(false)), on),
9394	        "a `/minimal` client attaching to a leader a full-screen pager started \
9395	         was charged for a row it cannot draw"
9396	    );
9397	    assert!(
9398	        wants_a_row(None, init_advertising_status_line(true)),
9399	        "with no leader the client that initialized is the client that asked"
9400	    );
9401	    assert!(!wants_a_row(None, init_advertising_status_line(false)));
9402	    assert!(
9403	        !wants_a_row(Some(acp::Meta::new()), says_nothing()),
9404	        "a leader that injected nothing leaves a silent client off"
9405	    );
9406	    assert!(!wants_a_row(None, says_nothing()));
9407	}
9408	#[tokio::test(flavor = "current_thread")]
9409	async fn an_attach_that_draws_a_row_switches_it_on_and_asks_for_a_fill() {
9410	    let agent = build_minimal_agent_for_tests();
9411	    let session_id = acp::SessionId::new("status-line-attach");
9412	    let (cmd_tx, mut commands) =
9413	        tokio::sync::mpsc::unbounded_channel::<crate::session::SessionCommand>();
9414	    let mut handle = make_test_handle("test-model", false, None);
9415	    handle.cmd_tx = cmd_tx;
9416	    let row = handle.client_caps.status_line.clone();
9417	    agent.insert_resident(&session_id, handle);
9418	    let init = init_advertising_status_line(false);
9419	    agent.attach_status_line(&session_id, Some(&status_line_meta(false)), &init);
9420	    assert!(
9421	        !row.load(std::sync::atomic::Ordering::Relaxed),
9422	        "an attach that cannot draw a row switched it on"
9423	    );
9424	    assert!(
9425	        commands.try_recv().is_err(),
9426	        "an attach that cannot draw a row asked the actor to build one"
9427	    );
9428	    agent.attach_status_line(&session_id, Some(&status_line_meta(true)), &init);
9429	    assert!(
9430	        row.load(std::sync::atomic::Ordering::Relaxed),
9431	        "the attach left the row off, so the emitter wakes and builds nothing"
9432	    );
9433	    assert!(
9434	        matches!(
9435	            commands.try_recv(),
9436	            Ok(crate::session::SessionCommand::EmitStatusSnapshot)
9437	        ),
9438	        "the attach never asked for a snapshot, so the transient row never fills"
9439	    );
9440	}
9441	/// A resident session outlives the client that drew its row, and the emitter re-reads the flag on every wake.
9442	/// A latch that only ever rose would keep building payloads for a row nobody paints.
9443	/// Driven through the real disconnect, not the setter, since the wiring is the part that can rot.
9444	#[test]
9445	fn a_disconnect_switches_the_row_off_and_the_next_attach_switches_it_on() {
9446	    run_local_for_bridge_test(|| async {
9447	        let agent = build_minimal_agent_for_tests();
9448	        let sid = acp::SessionId::new("sess-status-line-busy");
9449	        let (handle, _tx, rx) = make_live_session_handle(&sid, None);
9450	        let row = handle.client_caps.status_line.clone();
9451	        agent.insert_resident(&sid, handle);
9452	        let _actor = spawn_fake_actor(rx, true);
9453	        let init = init_advertising_status_line(true);
9454	        agent.attach_status_line(&sid, Some(&status_line_meta(true)), &init);
9455	        assert!(row.load(std::sync::atomic::Ordering::Relaxed));
9456	        drive_disconnect_many(&agent, &[&sid]).await;
9457	        assert!(
9458	            agent.is_resident(&sid),
9459	            "the busy session must stay resident"
9460	        );
9461	        assert!(
9462	            !row.load(std::sync::atomic::Ordering::Relaxed),
9463	            "the last client that could draw the row is gone, so the agent is \
9464	             still assembling payloads nobody paints"
9465	        );
9466	        agent.attach_status_line(&sid, Some(&status_line_meta(true)), &init);
9467	        assert!(
9468	            row.load(std::sync::atomic::Ordering::Relaxed),
9469	            "the row has to come back with the next client"
9470	        );
9471	    });
9472	}
9473	fn status_line_meta(enabled: bool) -> acp::Meta {
9474	    let mut meta = acp::Meta::new();
9475	    meta.insert(
9476	        xai_grok_status_line::CLIENT_STATUS_LINE_META.to_string(),
9477	        serde_json::json!(enabled),
9478	    );
9479	    meta
9480	}
9481	fn init_advertising_status_line(enabled: bool) -> acp::InitializeRequest {
9482	    let mut meta = serde_json::Map::new();
9483	    meta.insert(
9484	        xai_grok_status_line::STATUS_LINE_CAPABILITY.to_string(),
9485	        serde_json::json!(enabled),
9486	    );
9487	    acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
9488	        acp::ClientCapabilities::new()
9489	            .fs(acp::FileSystemCapabilities::new())
9490	            .terminal(false)
9491	            .meta(meta),
9492	    )
9493	}
9494	fn echo_session_meta(enabled: bool) -> acp::Meta {
9495	    let mut meta = acp::Meta::new();
9496	    meta.insert(
9497	        crate::session::CLIENT_USER_MESSAGE_ECHO_META.to_string(),
9498	        serde_json::json!(enabled),
9499	    );
9500	    meta
9501	}
9502	fn init_advertising_user_message_echo(enabled: bool) -> acp::InitializeRequest {
9503	    let mut meta = serde_json::Map::new();
9504	    meta.insert(
9505	        crate::session::USER_MESSAGE_ECHO_CAPABILITY.to_string(),
9506	        serde_json::json!(enabled),
9507	    );
9508	    acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
9509	        acp::ClientCapabilities::new()
9510	            .fs(acp::FileSystemCapabilities::new())
9511	            .terminal(false)
9512	            .meta(meta),
9513	    )
9514	}
9515	#[test]
9516	fn user_message_echo_session_meta_outranks_the_client_that_started_the_process() {
9517	    let says_nothing = || {
9518	        acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
9519	            acp::ClientCapabilities::new()
9520	                .fs(acp::FileSystemCapabilities::new())
9521	                .terminal(false),
9522	        )
9523	    };
9524	    let wanted = |meta: Option<acp::Meta>, init: acp::InitializeRequest| {
9525	        MvpAgent::resolve_user_message_echo_capability(meta.as_ref(), &init)
9526	    };
9527	    assert!(wanted(
9528	        Some(echo_session_meta(true)),
9529	        init_advertising_user_message_echo(false)
9530	    ));
9531	    assert!(!wanted(
9532	        Some(echo_session_meta(false)),
9533	        init_advertising_user_message_echo(true)
9534	    ));
9535	    assert!(wanted(None, init_advertising_user_message_echo(true)));
9536	    assert!(!wanted(None, init_advertising_user_message_echo(false)));
9537	    assert!(!wanted(Some(acp::Meta::new()), says_nothing()));
9538	    assert!(!wanted(None, says_nothing()));
9539	}
