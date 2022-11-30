use crate::process_finality_event;
use super::primitives::Chain;
use futures::StreamExt;


/// Core relayer loop, waits for new finality events and forwards any new [`ibc::IbcEvents`]
/// to the counter party chain.
pub async fn relay<A, B>(
	mut chain_a: A,
	mut chain_b: B,
) -> Result<(), anyhow::Error>
where
	A: Chain,
	B: Chain,
{
	let (mut chain_a_finality, mut chain_b_finality) =
		(chain_a.finality_notifications().await, chain_b.finality_notifications().await);
	// loop forever
	loop {
		tokio::select! {
			// new finality event from chain A
			result = chain_a_finality.next() => {
				process_finality_event!(chain_a, chain_b, result)
			}
			// new finality event from chain B
			result = chain_b_finality.next() => {
				process_finality_event!(chain_b, chain_a, result)
			}
		}
	}

	Ok(())
}