#[macro_export]
macro_rules! process_finality_event {
    ($source:ident, $sink:ident, $result:ident) => {
        match $result {
            // stream closed
            None => break,
            Some(finality_event) => {
                log::info!(target: "hyperspace-light", "📍 New finality event from {}", $source.name());
                let (_,events,_) = match $source
                .query_latest_ibc_events(finality_event.clone(), &$sink)
                    .await
                {
                    Ok(resp) => resp,
                    Err(err) => {
                        log::error!(
                            "Failed to fetch IBC events for finality event for {} {:?}",
                            $source.name(),
                            err
                        );
                        continue;
                    }
                };
                let event_types = events
                    .iter()
                    .map(|ev| ev.event.event_type())
                    .collect::<Vec<_>>();
                let (mut messages, timeouts, proof_height_ref) =
                    crate::core::events::parse_events(&mut $source, &mut $sink, events.clone()).await?;
                if !timeouts.is_empty() {
                    let type_urls = timeouts
                        .iter()
                        .map(|msg| msg.type_url.as_str())
                        .collect::<Vec<_>>();
                    log::info!(target: "hyperspace-light", "🧾 {} timeouts to be sent to {}", type_urls.len(), $sink.name());
                    crate::core::queue::flush_message_batch(timeouts, &$source).await?;
                }
                // We want to send client update if packet messages exist but where not sent due to
                // a connection delay even if client update message is optional
                log::info!(target: "hyperspace-light", "🧾 Messages to be sent to {}: {:?}", $sink.name(), event_types);
                while $source.latest_height_and_timestamp().await?.0 < proof_height_ref {
                    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
                }
                let (msg_update_client,_,update_type) = match $source
                .query_latest_ibc_events(finality_event.clone(), &$sink)
                .await
                {
                    Ok(resp) => resp,
                    Err(err) => {
                        log::error!(
                            "Failed to fetch IBC events for finality event for {} {:?}",
                            $source.name(),
                            err
                        );
                        continue;
                    }
                };
                match (
                    update_type.is_optional(),
                    crate::core::events::has_packet_events(&event_types),
                    messages.is_empty(),
                ) {
                    (true, false, true) => {
                        // skip sending ibc messages if no new events
                        log::info!(target: "hyperspace-light",
                            "🧾 Skipping finality notification for {}, No new events",
                            $source.name()
                        );
                        continue;
                    }
                    (false, _, true) => log::info!(target: "hyperspace-light",
                        "🧾 Sending mandatory client update message for {}",
                        $source.name()
                    ),
                    _ => log::info!(
                        target: "hyperspace-light", "🧾 Received finalized evenes: {:?}", event_types
                    ),
                };
                // insert client update at first position.
                messages.insert(0, msg_update_client);
                let type_urls = messages
                    .iter()
                    .map(|msg| msg.type_url.as_str())
                    .collect::<Vec<_>>();
                log::info!(target: "hyperspace-light", "🧾 {} messages to be sent to {}", messages.len(), $sink.name());
                log::info!(target: "hyperspace-light", "📡 Sending to {} following messages: {:?}", $sink.name(), type_urls);
                crate::core::queue::flush_message_batch(messages, &$sink).await?;
            }
        }
    };
}
