use super::*;

#[tokio::test]
async fn test_schedule_create_and_action_emit_ext_notifications_when_bridge_present() {
    let f = RealStorageHandleFixture::new().await;
    let session = f
        .storage
        .session_store()
        .create_session(None, None, None, None)
        .await
        .expect("create session");
    let session_id = session.public_id;

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    f.handle
        .set_bridge(crate::acp::client_bridge::ClientBridgeSender::new(tx))
        .await;

    let created = crate::control::schedules::create_schedule(
        &f.handle,
        crate::control::schedules::CreateScheduleControlRequest {
            node_id: None,
            session_id: session_id.clone(),
            prompt: "daily summary".to_string(),
            trigger: crate::session::domain_schedule::ScheduleTrigger::Interval { seconds: 300 },
            max_steps: None,
            max_cost_usd: None,
            max_runs: Some(2),
        },
    )
    .await
    .expect("create schedule");

    let first = rx.recv().await.expect("create notification");
    match first {
        crate::acp::client_bridge::ClientBridgeMessage::ExtNotification(notif) => {
            assert_eq!(notif.method.as_ref(), "querymt/schedules/changed");
            let value: serde_json::Value = serde_json::from_str(notif.params.get()).expect("json");
            assert_eq!(value["change"], "created");
            assert_eq!(value["schedule_public_id"], created.public_id);
            assert_eq!(value["session_id"], session_id);
        }
        _other => panic!("expected ext notification"),
    }

    crate::control::schedules::schedule_action(
        &f.handle,
        crate::control::schedules::ScheduleActionControlRequest {
            node_id: None,
            schedule_public_id: created.public_id.clone(),
        },
        "pause",
    )
    .await
    .expect("pause schedule");

    let second = rx.recv().await.expect("pause notification");
    match second {
        crate::acp::client_bridge::ClientBridgeMessage::ExtNotification(notif) => {
            assert_eq!(notif.method.as_ref(), "querymt/schedules/changed");
            let value: serde_json::Value = serde_json::from_str(notif.params.get()).expect("json");
            assert_eq!(value["change"], "updated");
            assert_eq!(value["schedule_public_id"], created.public_id);
        }
        _other => panic!("expected ext notification"),
    }
}
