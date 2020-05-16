use std::{
    boxed::Box,
    sync::Arc,
    time::Duration,
    future::Future,
    pin::Pin,
    task::{Context as FutContext, Poll},
};
use tokio::{
    sync::{
        mpsc::{
            unbounded_channel,
            UnboundedReceiver as Receiver,
            UnboundedSender as Sender,
        },
        Mutex,
    },
    time::{Delay, delay_for},
};
use futures::{
    future::BoxFuture,
    stream::{Stream, StreamExt},
};
use crate::{
    client::bridge::gateway::ShardMessenger,
    model::event::Event,
};

macro_rules! impl_generic_collector {
    ($($name:ident;)*) => {
        $(
            impl<'a> $name<'a> {
                /// Limits how many events will attempt to be filtered.
                ///
                /// The filter checks whether the message has been sent
                /// in the right guild, channel, and by the right author.
                pub fn filter_limit(mut self, limit: u32) -> Self {
                    self.filter.as_mut().unwrap().filter_limit = Some(limit);

                    self
                }

                /// Sets a filter function where events passed to the `function` must
                /// return `true`, otherwise the message won't be collected and failed the filter
                /// process.
                /// This is the last instance to pass for a message to count as *collected*.
                ///
                /// This function is intended to be a message content filter.
                pub fn filter<F: Fn(&Arc<Event>) -> bool + 'static + Send + Sync>(mut self, function: F) -> Self {
                    self.filter.as_mut().unwrap().filter = Some(Arc::new(function));

                    self
                }

                /// Sets the required author ID of a message.
                /// If a message does not meet this ID, it won't be received.
                pub fn author_id(mut self, author_id: impl Into<u64>) -> Self {
                    self.filter.as_mut().unwrap().author_id = Some(author_id.into());

                    self
                }

                /// Sets the required channel ID of a message.
                /// If a message does not meet this ID, it won't be received.
                pub fn channel_id(mut self, channel_id: impl Into<u64>) -> Self {
                    self.filter.as_mut().unwrap().channel_id = Some(channel_id.into());

                    self
                }

                /// Sets the required guild ID of a message.
                /// If a message does not meet this ID, it won't be received.
                pub fn guild_id(mut self, guild_id: impl Into<u64>) -> Self {
                    self.filter.as_mut().unwrap().guild_id = Some(guild_id.into());

                    self
                }

                /// Sets a `duration` for how long the collector shall receive
                /// events.
                pub fn timeout(mut self, duration: Duration) -> Self {
                    self.timeout = Some(delay_for(duration));

                    self
                }
            }
        )*
    }
}

/// Filters events on the shard's end and sends them to the collector.
#[derive(Clone, Debug)]
pub struct EventFilter {
    filtered: u32,
    collected: u32,
    options: FilterOptions,
    sender: Sender<Arc<Event>>,
}

impl EventFilter {
    /// Creates a new filter
    fn new(options: FilterOptions) -> (Self, Receiver<Arc<Event>>) {
        let (sender, receiver) = unbounded_channel();

        let filter = Self {
            filtered: 0,
            collected: 0,
            sender,
            options,
        };

        (filter, receiver)
    }

    /// Sends a `message` to the consuming collector if the `message` conforms
    /// to the constraints and the limits are not reached yet.
    pub(crate) fn send_message(&mut self, message: &Arc<Event>) -> bool {
        if self.is_passing_constraints(&message) {

            if self.options.filter.as_ref().map_or(true, |f| f(&message)) {
                self.collected += 1;

                if let Err(_) = self.sender.send(Arc::clone(message)) {
                    return false;
                }
            }
        }

        self.filtered += 1;

        self.is_within_limits()
    }

    /// Checks if the `message` passes set constraints.
    /// Constraints are optional, as it is possible to limit events to
    /// be sent by a specific author or in a specifc guild.
    fn is_passing_constraints(&self, message: &Arc<Event>) -> bool {
        self.options.guild_id.map_or(true, |g| { Some(g) == message.guild_id.map(|g| g.0) })
        && self.options.channel_id.map_or(true, |g| { g == message.channel_id.0 })
        && self.options.author_id.map_or(true, |g| { g == message.author.id.0 })
    }

    /// Checks if the filter is within set receive and collect limits.
    /// A message is considered *received* even when it does not meet the
    /// constraints.
    fn is_within_limits(&self) -> bool {
        self.options.filter_limit.as_ref().map_or(true, |limit| { self.filtered < *limit })
        && self.options.collect_limit.as_ref().map_or(true, |limit| { self.collected < *limit })
    }
}


#[derive(Clone, Default)]
struct FilterOptions {
    filter_limit: Option<u32>,
    collect_limit: Option<u32>,
    filter: Option<Arc<dyn Fn(&Arc<Event>) -> bool + 'static + Send + Sync>>,
    channel_id: Option<u64>,
    guild_id: Option<u64>,
    author_id: Option<u64>,
}

// Implement the common setters for all message collector types.
impl_generic_collector! {
    CollectEvent;
    EventCollectorBuilder;
}

/// Future building a stream of events.
pub struct EventCollectorBuilder<'a> {
    filter: Option<FilterOptions>,
    shard: Option<ShardMessenger>,
    timeout: Option<Delay>,
    fut: Option<BoxFuture<'a, EventCollector>>,
}

impl<'a> EventCollectorBuilder<'a> {
    /// A future that builds a [`EventCollector`] based on the settings.
    ///
    /// [`EventCollector`]: ../struct.EventCollector.html
    pub fn new(shard_messenger: impl AsRef<ShardMessenger>) -> Self {
        Self {
            filter: Some(FilterOptions::default()),
            shard: Some(shard_messenger.as_ref().clone()),
            timeout: None,
            fut: None,
        }
    }

    /// Limits how many events can be collected.
    ///
    /// A message is considered *collected*, if the message
    /// passes all the requirements.
    pub fn collect_limit(mut self, limit: u32) -> Self {
        self.filter.as_mut().unwrap().collect_limit = Some(limit);

        self
    }
}

impl<'a> Future for EventCollectorBuilder<'a> {
    type Output = EventCollector;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut FutContext<'_>) -> Poll<Self::Output> {
        if self.fut.is_none() {
            let shard_messenger = self.shard.take().unwrap();
            let (filter, receiver) = EventFilter::new(self.filter.take().unwrap());
            let timeout = self.timeout.take();

            self.fut = Some(Box::pin(async move {
                shard_messenger.set_message_filter(filter);

                EventCollector {
                    receiver: Box::pin(receiver),
                    timeout: timeout.map(Box::pin),
                }
            }))
        }

        self.fut.as_mut().unwrap().as_mut().poll(ctx)
    }
}

pub struct CollectEvent<'a> {
    filter: Option<FilterOptions>,
    shard: Option<ShardMessenger>,
    timeout: Option<Delay>,
    fut: Option<BoxFuture<'a, Option<Arc<Event>>>>,
}

impl<'a> CollectEvent<'a> {
    pub fn new(shard_messenger: impl AsRef<ShardMessenger>) -> Self {
        Self {
            filter: Some(FilterOptions::default()),
            shard: Some((shard_messenger.as_ref()).clone()),
            timeout: None,
            fut: None,
        }
    }
}

impl<'a> Future for CollectEvent<'a> {
    type Output = Option<Arc<Event>>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut FutContext<'_>) -> Poll<Self::Output> {
        if self.fut.is_none() {
            let shard_messenger = self.shard.take().unwrap();
            let (filter, receiver) = EventFilter::new(self.filter.take().unwrap());
            let timeout = self.timeout.take();

            self.fut = Some(Box::pin(async move {
                shard_messenger.set_message_filter(filter);

                EventCollector {
                    receiver: Box::pin(receiver),
                    timeout: timeout.map(Box::pin),
                }.next().await
            }))
        }

        self.fut.as_mut().unwrap().as_mut().poll(ctx)
    }
}

impl std::fmt::Debug for FilterOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventFilter")
            .field("collect_limit", &self.collect_limit)
            .field("filter", &"Option<Arc<dyn Fn(&Arc<Event>) -> bool + 'static + Send + Sync>>")
            .field("channel_id", &self.channel_id)
            .field("guild_id", &self.guild_id)
            .field("author_id", &self.author_id)
            .finish()
    }
}

/// A message collector receives events matching a the given filter for a
/// set duration.
pub struct EventCollector {
    receiver: Pin<Box<Receiver<Arc<Event>>>>,
    timeout: Option<Pin<Box<Delay>>>,
}

impl EventCollector {
    /// Stops collecting, this will implicitly be done once the
    /// collector drops.
    /// In case the drop does not appear until later, it is preferred to
    /// stop the collector early.
    pub fn stop(mut self) {
        self.receiver.close();
    }
}

impl Stream for EventCollector {
    type Item = Arc<Event>;
    fn poll_next(mut self: Pin<&mut Self>, ctx: &mut FutContext<'_>) -> Poll<Option<Self::Item>> {
        if let Some(ref mut timeout) = self.timeout {

            match timeout.as_mut().poll(ctx) {
                Poll::Ready(_) => {
                    return Poll::Ready(None);
                },
                Poll::Pending => (),
            }
        }

        self.receiver.as_mut().poll_next(ctx)
    }
}

impl Drop for EventCollector {
    fn drop(&mut self) {
        self.receiver.close();
    }
}