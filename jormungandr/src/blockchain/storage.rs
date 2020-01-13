use crate::{
    blockcfg::{Block, HeaderHash},
    start_up::NodeStorage,
};
use chain_storage_sqlite_old::{for_path_to_nth_ancestor, BlockInfo};
use tokio::prelude::future::Either;
use tokio::prelude::*;
use tokio::sync::lock::Lock;

pub use chain_storage_sqlite_old::Error as StorageError;

#[derive(Clone)]
pub struct Storage {
    // This must be used only for read operations.
    read_connection: NodeStorage,
    // All write operations must be performed only via this lock. The lock helps
    // us to ensure that all of the write operations are performed in the right
    // sequence. Otherwise they can be performed out of the expected order (for
    // example, by different tokio executors) which eventually leads to a panic
    // because the block data would be inconsistent at the time of a write.
    write_connection_lock: Lock<NodeStorage>,
}

pub struct BlockStream {
    inner: NodeStorage,
    state: BlockIterState,
}

pub struct Ancestor {
    pub header_hash: HeaderHash,
    pub distance: u64,
}

struct BlockIterState {
    to_length: u64,
    cur_length: u64,
    pending_infos: Vec<BlockInfo<HeaderHash>>,
}

impl Storage {
    pub fn new(storage: NodeStorage) -> Self {
        Storage {
            read_connection: storage.clone(),
            write_connection_lock: Lock::new(storage),
        }
    }

    #[deprecated(since = "new blockchain API", note = "use the stream iterator instead")]
    pub fn get_inner(&self) -> impl Future<Item = NodeStorage, Error = StorageError> {
        future::ok(self.read_connection.clone())
    }

    pub fn get_tag(
        &self,
        tag: String,
    ) -> impl Future<Item = Option<HeaderHash>, Error = StorageError> {
        future::result(self.read_connection.get_tag(&tag))
    }

    pub fn put_tag(
        &mut self,
        tag: String,
        header_hash: HeaderHash,
    ) -> impl Future<Item = (), Error = StorageError> {
        let mut write_connection_lock = self.write_connection_lock.clone();

        future::poll_fn(move || Ok(write_connection_lock.poll_lock())).and_then(move |mut guard| {
            match guard.put_tag(&tag, &header_hash) {
                Err(error) => future::err(error),
                Ok(res) => future::ok(res),
            }
        })
    }

    pub fn get(
        &self,
        header_hash: HeaderHash,
    ) -> impl Future<Item = Option<Block>, Error = StorageError> {
        match self.read_connection.get_block(&header_hash) {
            Err(StorageError::BlockNotFound) => future::ok(None),
            Err(error) => future::err(error),
            Ok((block, _block_info)) => future::ok(Some(block)),
        }
    }

    pub fn get_with_info(
        &self,
        header_hash: HeaderHash,
    ) -> impl Future<Item = Option<(Block, BlockInfo<HeaderHash>)>, Error = StorageError> {
        match self.read_connection.get_block(&header_hash) {
            Err(StorageError::BlockNotFound) => future::ok(None),
            Err(error) => future::err(error),
            Ok(v) => future::ok(Some(v)),
        }
    }

    pub fn block_exists(
        &self,
        header_hash: HeaderHash,
    ) -> impl Future<Item = bool, Error = StorageError> {
        match self.read_connection.block_exists(&header_hash) {
            Err(StorageError::BlockNotFound) => future::ok(false),
            Err(error) => future::err(error),
            Ok(existence) => future::ok(existence),
        }
    }

    pub fn put_block(&mut self, block: Block) -> impl Future<Item = (), Error = StorageError> {
        let mut write_connection_lock = self.write_connection_lock.clone();

        future::poll_fn(move || Ok(write_connection_lock.poll_lock())).and_then(move |mut guard| {
            match guard.put_block(&block) {
                Err(StorageError::BlockNotFound) => unreachable!(),
                Err(error) => future::err(error),
                Ok(()) => future::ok(()),
            }
        })
    }

    /// Return values:
    /// - `Ok(stream)` - `from` is ancestor of `to`, returns blocks between them
    /// - `Err(CannotIterate)` - `from` is not ancestor of `to`
    /// - `Err(BlockNotFound)` - `from` or `to` was not found
    /// - `Err(_)` - some other storage error
    pub fn stream_from_to(
        &self,
        from: HeaderHash,
        to: HeaderHash,
    ) -> impl Future<Item = BlockStream, Error = StorageError> {
        match self.read_connection.is_ancestor(&from, &to) {
            Err(error) => future::err(error),
            Ok(None) => future::err(StorageError::CannotIterate),
            Ok(Some(distance)) => match self.read_connection.get_block_info(&to) {
                Err(error) => future::err(error),
                Ok(to_info) => future::ok(BlockStream {
                    inner: self.read_connection.clone(),
                    state: BlockIterState::new(to_info, distance),
                }),
            },
        }
    }

    /// Stream a branch ending at `to` and starting from the ancestor
    /// at `depth` or at the first ancestor since genesis block
    /// if `depth` is given as `None`.
    ///
    /// This function uses buffering in the sink to reduce lock contention.
    pub fn send_branch<S, E>(
        &self,
        to: HeaderHash,
        depth: Option<u64>,
        sink: S,
    ) -> impl Future<Item = (), Error = S::SinkError>
    where
        S: Sink<SinkItem = Result<Block, E>>,
        E: From<StorageError>,
    {
        let res = self.read_connection.get_block_info(&to).map(|to_info| {
            let depth = depth.unwrap_or(to_info.chain_length - 1);
            BlockIterState::new(to_info, depth)
        });

        match res {
            Ok(iter) => {
                let mut state = SendState {
                    sink,
                    iter,
                    pending: None,
                };
                let mut store = self.read_connection.clone();
                let fut = future::poll_fn(move || {
                    while try_ready!(state.poll_continue()) {
                        try_ready!(state.fill_sink(&mut store));
                    }
                    Ok(().into())
                });
                Either::A(fut)
            }
            Err(e) => {
                let fut = sink
                    .send_all(stream::once(Ok(Err(e.into()))))
                    .map(|(_, _)| ());
                Either::B(fut)
            }
        }
    }

    pub fn find_closest_ancestor(
        &self,
        checkpoints: Vec<HeaderHash>,
        descendant: HeaderHash,
    ) -> impl Future<Item = Option<Ancestor>, Error = StorageError> {
        let mut ancestor = None;
        let mut closest_found = std::u64::MAX;
        for checkpoint in checkpoints {
            // Checkpoints sent by a peer may not
            // be present locally, so we need to ignore certain errors
            match self.read_connection.is_ancestor(&checkpoint, &descendant) {
                Ok(None) => {}
                Ok(Some(distance)) => {
                    if closest_found > distance {
                        ancestor = Some(checkpoint);
                        closest_found = distance;
                    }
                }
                Err(e) => {
                    // Checkpoints sent by a peer may not
                    // be present locally, so we need to ignore certain errors
                    match e {
                        StorageError::BlockNotFound => {
                            // FIXME: add block hash into the error so we
                            // can see which of the two it is.
                            // For now, just ignore either.
                        }
                        _ => return future::err(e),
                    }
                }
            }
        }
        future::ok(ancestor.map(|header_hash| Ancestor {
            header_hash,
            distance: closest_found,
        }))
    }
}

impl Stream for BlockStream {
    type Item = Block;
    type Error = StorageError;

    fn poll(&mut self) -> Poll<Option<Block>, Self::Error> {
        if !self.state.has_next() {
            return Ok(Async::Ready(None));
        }

        self.state
            .get_next(&mut self.inner)
            .map(|block| Async::Ready(Some(block)))
    }
}

impl BlockIterState {
    fn new(to_info: BlockInfo<HeaderHash>, distance: u64) -> Self {
        BlockIterState {
            to_length: to_info.chain_length,
            cur_length: to_info.chain_length - distance,
            pending_infos: vec![to_info],
        }
    }

    fn has_next(&self) -> bool {
        self.cur_length < self.to_length
    }

    fn get_next(&mut self, store: &mut NodeStorage) -> Result<Block, StorageError> {
        assert!(self.has_next());

        self.cur_length += 1;

        let block_info = self.pending_infos.pop().unwrap();

        if block_info.chain_length == self.cur_length {
            // We've seen this block on a previous ancestor traversal.
            let (block, _block_info) = store.get_block(&block_info.block_hash)?;
            Ok(block)
        } else {
            // We don't have this block yet, so search back from
            // the furthest block that we do have.
            assert!(self.cur_length < block_info.chain_length);
            let chain_length = block_info.chain_length;
            let parent = block_info.parent_id();
            self.pending_infos.push(block_info);
            let block_info = for_path_to_nth_ancestor(
                &*store,
                &parent,
                chain_length - self.cur_length - 1,
                |new_info| {
                    self.pending_infos.push(new_info.clone());
                },
            )?;

            let (block, _block_info) = store.get_block(&block_info.block_hash)?;
            Ok(block)
        }
    }
}

struct SendState<S, E> {
    sink: S,
    iter: BlockIterState,
    pending: Option<Result<Block, E>>,
}

impl<S, E> SendState<S, E>
where
    S: Sink<SinkItem = Result<Block, E>>,
    E: From<StorageError>,
{
    fn poll_continue(&mut self) -> Poll<bool, S::SinkError> {
        if let Some(item) = self.pending.take() {
            match self.sink.start_send(item)? {
                AsyncSink::Ready => {}
                AsyncSink::NotReady(item) => {
                    self.pending = Some(item);
                    return Ok(Async::NotReady);
                }
            }
        }

        let has_next = self.iter.has_next();

        if has_next {
            // Flush the sink before locking to send more blocks
            try_ready!(self.sink.poll_complete());
        } else {
            try_ready!(self.sink.close());
        }

        Ok(has_next.into())
    }

    fn fill_sink(&mut self, store: &mut NodeStorage) -> Poll<(), S::SinkError> {
        assert!(self.iter.has_next());
        loop {
            let item = self.iter.get_next(store).map_err(Into::into);
            match self.sink.start_send(item)? {
                AsyncSink::Ready => {
                    if !self.iter.has_next() {
                        return Ok(().into());
                    } else {
                        // FIXME: have to yield and release the storage lock
                        // because .get_next() may block on database access,
                        // starving other storage access queries.
                        // https://github.com/input-output-hk/jormungandr/issues/1263
                        task::current().notify();
                        return Ok(Async::NotReady);
                    }
                }
                AsyncSink::NotReady(item) => {
                    self.pending = Some(item);
                    return Ok(Async::NotReady);
                }
            }
        }
    }
}
