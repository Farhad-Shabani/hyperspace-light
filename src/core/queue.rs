// Copyright 2022 ComposableFi
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::primitives::Chain;
use ibc_proto::google::protobuf::Any;

/// This sends messages to the sink chain in a gas-aware manner.
pub async fn flush_message_batch(msgs: Vec<Any>, sink: &impl Chain) -> Result<(), anyhow::Error> {
    let block_max_weight = sink.block_max_weight();
    let batch_weight = sink.estimate_weight(msgs.clone()).await?;

    let ratio = (batch_weight / block_max_weight) as usize;
    if ratio == 0 {
        let tx_id = sink.submit(msgs).await?;
        log::info!(target: "hyperspace-light", "🤝 Transaction flushed successfully with hash: {:?}", tx_id);
        return Ok(());
    }

    // whelp our batch exceeds the block max weight.
    let chunk = if ratio == 1 {
        // split the batch into ratio * 2
        ratio * 2
    } else {
        // split the batch into ratio + 2
        ratio + 2
    };

    log::info!(
        target: "hyperspace-light",
        "🏗️🏗️🏗️ Splitting batch into {} chunks",
        chunk
    );
    for batch in msgs.chunks(chunk) {
        // send out batches.
        log::info!(target: "hyperspace-light", "📡 Sending batch of {} messages", batch.len());
        let tx_id = sink.submit(batch.to_vec()).await?;
        log::info!(target: "hyperspace-light", "🤝 Transaction confirmed with hash: {:?}", tx_id);
    }

    Ok(())
}
