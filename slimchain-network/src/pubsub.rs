use libp2p::{
    gossipsub::{
        Gossipsub, GossipsubConfigBuilder, GossipsubEvent, GossipsubMessage, MessageAuthenticity,
        MessageId, Topic, TopicHash,
    },
    identity::Keypair,
    swarm::{NetworkBehaviourAction, NetworkBehaviourEventProcess, PollParameters},
    NetworkBehaviour,
};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use slimchain_common::{
    collections::{HashMap, HashSet},
    digest::Digestible,
    error::{anyhow, ensure, Result},
};
use std::{
    collections::VecDeque,
    task::{Context, Poll},
    time::Duration,
};

const MAX_MESSAGE_SIZE: usize = 50_000_000;
const MAX_TRANSMIT_SIZE: usize = (MAX_MESSAGE_SIZE as f64 * 1.1) as usize;
const DUPLICATE_CACHE_TTL: Duration = Duration::from_secs(300);

static TOPIC_MAP: Lazy<HashMap<TopicHash, PubSubTopic>> = Lazy::new(|| {
    let mut map = HashMap::with_capacity(2);
    for &topic in &[PubSubTopic::TxProposal, PubSubTopic::BlockProposal] {
        map.insert(topic.into_topic_hash(), topic);
    }
    map
});

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
pub enum PubSubTopic {
    TxProposal,
    BlockProposal,
}

impl PubSubTopic {
    pub fn into_topic(self) -> Topic {
        match self {
            PubSubTopic::TxProposal => Topic::new("tx_proposal".to_string()),
            PubSubTopic::BlockProposal => Topic::new("block_proposal".to_string()),
        }
    }

    pub fn into_topic_hash(self) -> TopicHash {
        self.into_topic().no_hash()
    }
}

#[derive(Debug)]
pub enum PubSubEvent<TxProposal, BlockProposal> {
    TxProposal(TxProposal),
    BlockProposal(BlockProposal),
}

#[derive(NetworkBehaviour)]
#[behaviour(
    poll_method = "poll_inner",
    out_event = "PubSubEvent<TxProposal, BlockProposal>"
)]
pub struct PubSub<TxProposal, BlockProposal>
where
    TxProposal: Send + 'static,
    BlockProposal: Send + 'static,
{
    gossipsub: Gossipsub,
    #[behaviour(ignore)]
    pending_events: VecDeque<PubSubEvent<TxProposal, BlockProposal>>,
    #[behaviour(ignore)]
    topics: HashSet<PubSubTopic>,
}

impl<TxProposal, BlockProposal> PubSub<TxProposal, BlockProposal>
where
    TxProposal: Send + 'static,
    BlockProposal: Send + 'static,
{
    pub fn new(keypair: Keypair, topics: &[PubSubTopic]) -> Self {
        let cfg = GossipsubConfigBuilder::default()
            .protocol_id(&b"/slimchain/pubsub/1"[..])
            .duplicate_cache_time(DUPLICATE_CACHE_TTL)
            .message_id_fn(|msg: &GossipsubMessage| {
                let hash = msg.data.to_digest();
                MessageId::new(hash.as_bytes())
            })
            .max_transmit_size(MAX_TRANSMIT_SIZE)
            .build();
        let mut gossipsub = Gossipsub::new(MessageAuthenticity::Signed(keypair), cfg);
        for topic in &[PubSubTopic::BlockProposal, PubSubTopic::TxProposal] {
            gossipsub.subscribe(topic.into_topic());
        }

        Self {
            gossipsub,
            pending_events: VecDeque::new(),
            topics: topics.iter().copied().collect(),
        }
    }

    fn poll_inner<T>(
        &mut self,
        _: &mut Context,
        _: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<T, PubSubEvent<TxProposal, BlockProposal>>> {
        if let Some(event) = self.pending_events.pop_front() {
            return Poll::Ready(NetworkBehaviourAction::GenerateEvent(event));
        }

        Poll::Pending
    }

    pub fn report_known_peers(&self) {
        println!("[PubSub] Known peers:");
        for topic in &[PubSubTopic::TxProposal, PubSubTopic::BlockProposal] {
            println!(
                " {:?} => {:#?}",
                topic,
                self.gossipsub
                    .peers(&topic.into_topic_hash())
                    .collect::<Vec<_>>()
            );
        }
    }
}

impl<TxProposal, BlockProposal> PubSub<TxProposal, BlockProposal>
where
    TxProposal: Serialize + Send + 'static,
    BlockProposal: Serialize + Send + 'static,
{
    pub fn publish_tx_proposal(&mut self, input: &TxProposal) -> Result<()> {
        let data = postcard::to_allocvec(input)?;
        ensure!(
            data.len() < MAX_MESSAGE_SIZE,
            "PubSub: data is too large. Size={}.",
            data.len()
        );
        self.gossipsub
            .publish(&PubSubTopic::TxProposal.into_topic(), data)
            .map_err(|e| anyhow!("PubSub: Failed to publish tx proposal. Error: {:?}", e))
    }

    pub fn publish_block_proposal(&mut self, input: &BlockProposal) -> Result<()> {
        let data = postcard::to_allocvec(input)?;
        ensure!(
            data.len() < MAX_MESSAGE_SIZE,
            "PubSub: data is too large. Size={}.",
            data.len()
        );
        self.gossipsub
            .publish(&PubSubTopic::BlockProposal.into_topic(), data)
            .map_err(|e| anyhow!("PubSub: Failed to publish block proposal. Error: {:?}", e))
    }
}

impl<TxProposal, BlockProposal> NetworkBehaviourEventProcess<GossipsubEvent>
    for PubSub<TxProposal, BlockProposal>
where
    TxProposal: for<'de> Deserialize<'de> + Send + 'static,
    BlockProposal: for<'de> Deserialize<'de> + Send + 'static,
{
    fn inject_event(&mut self, event: GossipsubEvent) {
        if let GossipsubEvent::Message(_, _, GossipsubMessage { data, topics, .. }) = event {
            let topic = match TOPIC_MAP.get(&topics[0]) {
                Some(topic) => topic,
                None => {
                    warn!(?topics, "PubSub: Unknown topic.");
                    return;
                }
            };

            if !self.topics.contains(topic) {
                return;
            }

            match topic {
                PubSubTopic::TxProposal => {
                    let input = postcard::from_bytes(data.as_slice())
                        .expect("PubSub: Failed to decode message.");
                    self.pending_events
                        .push_back(PubSubEvent::TxProposal(input));
                }
                PubSubTopic::BlockProposal => {
                    let input = postcard::from_bytes(data.as_slice())
                        .expect("PubSub: Failed to decode message.");
                    self.pending_events
                        .push_back(PubSubEvent::BlockProposal(input));
                }
            }
        }
    }
}
