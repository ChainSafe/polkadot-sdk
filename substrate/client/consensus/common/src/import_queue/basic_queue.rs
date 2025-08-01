// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.
use futures::{
	prelude::*,
	task::{Context, Poll},
};
use log::{debug, trace};
use prometheus_endpoint::Registry;
use sc_utils::mpsc::{tracing_unbounded, TracingUnboundedReceiver, TracingUnboundedSender};
use sp_consensus::BlockOrigin;
use sp_runtime::{
	traits::{Block as BlockT, Header as HeaderT, NumberFor},
	Justification, Justifications,
};
use std::pin::Pin;

use crate::{
	import_queue::{
		buffered_link::{self, BufferedLinkReceiver, BufferedLinkSender},
		import_single_block_metered, verify_single_block_metered, BlockImportError,
		BlockImportStatus, BoxBlockImport, BoxJustificationImport, ImportQueue, ImportQueueService,
		IncomingBlock, JustificationImportResult, Link, RuntimeOrigin,
		SingleBlockVerificationOutcome, Verifier, LOG_TARGET,
	},
	metrics::Metrics,
};

/// Interface to a basic block import queue that is importing blocks sequentially in a separate
/// task, with plugable verification.
pub struct BasicQueue<B: BlockT> {
	/// Handle for sending justification and block import messages to the background task.
	handle: BasicQueueHandle<B>,
	/// Results coming from the worker task.
	result_port: BufferedLinkReceiver<B>,
}

impl<B: BlockT> Drop for BasicQueue<B> {
	fn drop(&mut self) {
		// Flush the queue and close the receiver to terminate the future.
		self.handle.close();
		self.result_port.close();
	}
}

impl<B: BlockT> BasicQueue<B> {
	/// Instantiate a new basic queue, with given verifier.
	///
	/// This creates a background task, and calls `on_start` on the justification importer.
	pub fn new<V>(
		verifier: V,
		block_import: BoxBlockImport<B>,
		justification_import: Option<BoxJustificationImport<B>>,
		spawner: &impl sp_core::traits::SpawnEssentialNamed,
		prometheus_registry: Option<&Registry>,
	) -> Self
	where
		V: Verifier<B> + 'static,
	{
		let (result_sender, result_port) = buffered_link::buffered_link(100_000);

		let metrics = prometheus_registry.and_then(|r| {
			Metrics::register(r)
				.map_err(|err| {
					log::warn!("Failed to register Prometheus metrics: {}", err);
				})
				.ok()
		});

		let (future, justification_sender, block_import_sender) = BlockImportWorker::new(
			result_sender,
			verifier,
			block_import,
			justification_import,
			metrics,
		);

		spawner.spawn_essential_blocking(
			"basic-block-import-worker",
			Some("block-import"),
			future.boxed(),
		);

		Self {
			handle: BasicQueueHandle::new(justification_sender, block_import_sender),
			result_port,
		}
	}
}

#[derive(Clone)]
struct BasicQueueHandle<B: BlockT> {
	/// Channel to send justification import messages to the background task.
	justification_sender: TracingUnboundedSender<worker_messages::ImportJustification<B>>,
	/// Channel to send block import messages to the background task.
	block_import_sender: TracingUnboundedSender<worker_messages::ImportBlocks<B>>,
}

impl<B: BlockT> BasicQueueHandle<B> {
	pub fn new(
		justification_sender: TracingUnboundedSender<worker_messages::ImportJustification<B>>,
		block_import_sender: TracingUnboundedSender<worker_messages::ImportBlocks<B>>,
	) -> Self {
		Self { justification_sender, block_import_sender }
	}

	pub fn close(&mut self) {
		self.justification_sender.close();
		self.block_import_sender.close();
	}
}

impl<B: BlockT> ImportQueueService<B> for BasicQueueHandle<B> {
	fn import_blocks(&mut self, origin: BlockOrigin, blocks: Vec<IncomingBlock<B>>) {
		if blocks.is_empty() {
			return
		}

		trace!(target: LOG_TARGET, "Scheduling {} blocks for import", blocks.len());
		let res = self
			.block_import_sender
			.unbounded_send(worker_messages::ImportBlocks(origin, blocks));

		if res.is_err() {
			log::error!(
				target: LOG_TARGET,
				"import_blocks: Background import task is no longer alive"
			);
		}
	}

	fn import_justifications(
		&mut self,
		who: RuntimeOrigin,
		hash: B::Hash,
		number: NumberFor<B>,
		justifications: Justifications,
	) {
		for justification in justifications {
			let res = self.justification_sender.unbounded_send(
				worker_messages::ImportJustification(who, hash, number, justification),
			);

			if res.is_err() {
				log::error!(
					target: LOG_TARGET,
					"import_justification: Background import task is no longer alive"
				);
			}
		}
	}
}

#[async_trait::async_trait]
impl<B: BlockT> ImportQueue<B> for BasicQueue<B> {
	/// Get handle to [`ImportQueueService`].
	fn service(&self) -> Box<dyn ImportQueueService<B>> {
		Box::new(self.handle.clone())
	}

	/// Get a reference to the handle to [`ImportQueueService`].
	fn service_ref(&mut self) -> &mut dyn ImportQueueService<B> {
		&mut self.handle
	}

	/// Poll actions from network.
	fn poll_actions(&mut self, cx: &mut Context, link: &dyn Link<B>) {
		if self.result_port.poll_actions(cx, link).is_err() {
			log::error!(
				target: LOG_TARGET,
				"poll_actions: Background import task is no longer alive"
			);
		}
	}

	/// Start asynchronous runner for import queue.
	///
	/// Takes an object implementing [`Link`] which allows the import queue to
	/// influence the synchronization process.
	async fn run(mut self, link: &dyn Link<B>) {
		loop {
			if let Err(_) = self.result_port.next_action(link).await {
				log::error!(target: "sync", "poll_actions: Background import task is no longer alive");
				return
			}
		}
	}
}

/// Messages designated to the background worker.
mod worker_messages {
	use super::*;

	pub struct ImportBlocks<B: BlockT>(pub BlockOrigin, pub Vec<IncomingBlock<B>>);
	pub struct ImportJustification<B: BlockT>(
		pub RuntimeOrigin,
		pub B::Hash,
		pub NumberFor<B>,
		pub Justification,
	);
}

/// The process of importing blocks.
///
/// This polls the `block_import_receiver` for new blocks to import and than awaits on
/// importing these blocks. After each block is imported, this async function yields once
/// to give other futures the possibility to be run.
///
/// Returns when `block_import` ended.
async fn block_import_process<B: BlockT>(
	mut block_import: BoxBlockImport<B>,
	verifier: impl Verifier<B>,
	result_sender: BufferedLinkSender<B>,
	mut block_import_receiver: TracingUnboundedReceiver<worker_messages::ImportBlocks<B>>,
	metrics: Option<Metrics>,
) {
	loop {
		let worker_messages::ImportBlocks(origin, blocks) = match block_import_receiver.next().await
		{
			Some(blocks) => blocks,
			None => {
				log::debug!(
					target: LOG_TARGET,
					"Stopping block import because the import channel was closed!",
				);
				return
			},
		};

		let res =
			import_many_blocks(&mut block_import, origin, blocks, &verifier, metrics.clone()).await;

		result_sender.blocks_processed(res.imported, res.block_count, res.results);
	}
}

struct BlockImportWorker<B: BlockT> {
	result_sender: BufferedLinkSender<B>,
	justification_import: Option<BoxJustificationImport<B>>,
	metrics: Option<Metrics>,
}

impl<B: BlockT> BlockImportWorker<B> {
	fn new<V>(
		result_sender: BufferedLinkSender<B>,
		verifier: V,
		block_import: BoxBlockImport<B>,
		justification_import: Option<BoxJustificationImport<B>>,
		metrics: Option<Metrics>,
	) -> (
		impl Future<Output = ()> + Send,
		TracingUnboundedSender<worker_messages::ImportJustification<B>>,
		TracingUnboundedSender<worker_messages::ImportBlocks<B>>,
	)
	where
		V: Verifier<B> + 'static,
	{
		use worker_messages::*;

		let (justification_sender, mut justification_port) =
			tracing_unbounded("mpsc_import_queue_worker_justification", 100_000);

		let (block_import_sender, block_import_receiver) =
			tracing_unbounded("mpsc_import_queue_worker_blocks", 100_000);

		let mut worker = BlockImportWorker { result_sender, justification_import, metrics };

		let future = async move {
			// Let's initialize `justification_import`
			if let Some(justification_import) = worker.justification_import.as_mut() {
				for (hash, number) in justification_import.on_start().await {
					worker.result_sender.request_justification(&hash, number);
				}
			}

			let block_import_process = block_import_process(
				block_import,
				verifier,
				worker.result_sender.clone(),
				block_import_receiver,
				worker.metrics.clone(),
			);
			futures::pin_mut!(block_import_process);

			loop {
				// If the results sender is closed, that means that the import queue is shutting
				// down and we should end this future.
				if worker.result_sender.is_closed() {
					log::debug!(
						target: LOG_TARGET,
						"Stopping block import because result channel was closed!",
					);
					return
				}

				// Make sure to first process all justifications
				while let Poll::Ready(justification) = futures::poll!(justification_port.next()) {
					match justification {
						Some(ImportJustification(who, hash, number, justification)) =>
							worker.import_justification(who, hash, number, justification).await,
						None => {
							log::debug!(
								target: LOG_TARGET,
								"Stopping block import because justification channel was closed!",
							);
							return
						},
					}
				}

				if let Poll::Ready(()) = futures::poll!(&mut block_import_process) {
					return
				}

				// All futures that we polled are now pending.
				futures::pending!()
			}
		};

		(future, justification_sender, block_import_sender)
	}

	async fn import_justification(
		&mut self,
		who: RuntimeOrigin,
		hash: B::Hash,
		number: NumberFor<B>,
		justification: Justification,
	) {
		let started = std::time::Instant::now();

		let import_result = match self.justification_import.as_mut() {
			Some(justification_import) => {
				let result = justification_import
				.import_justification(hash, number, justification)
				.await
				.map_err(|e| {
					debug!(
						target: LOG_TARGET,
						"Justification import failed for hash = {:?} with number = {:?} coming from node = {:?} with error: {}",
						hash,
						number,
						who,
						e,
					);
					e
				});
				match result {
					Ok(()) => JustificationImportResult::Success,
					Err(sp_consensus::Error::OutdatedJustification) =>
						JustificationImportResult::OutdatedJustification,
					Err(_) => JustificationImportResult::Failure,
				}
			},
			None => JustificationImportResult::Failure,
		};

		if let Some(metrics) = self.metrics.as_ref() {
			metrics.justification_import_time.observe(started.elapsed().as_secs_f64());
		}

		self.result_sender.justification_imported(who, &hash, number, import_result);
	}
}

/// Result of [`import_many_blocks`].
struct ImportManyBlocksResult<B: BlockT> {
	/// The number of blocks imported successfully.
	imported: usize,
	/// The total number of blocks processed.
	block_count: usize,
	/// The import results for each block.
	results: Vec<(Result<BlockImportStatus<NumberFor<B>>, BlockImportError>, B::Hash)>,
}

/// Import several blocks at once, returning import result for each block.
///
/// This will yield after each imported block once, to ensure that other futures can
/// be called as well.
async fn import_many_blocks<B: BlockT, V: Verifier<B>>(
	import_handle: &mut BoxBlockImport<B>,
	blocks_origin: BlockOrigin,
	blocks: Vec<IncomingBlock<B>>,
	verifier: &V,
	metrics: Option<Metrics>,
) -> ImportManyBlocksResult<B> {
	let count = blocks.len();

	let blocks_range = match (
		blocks.first().and_then(|b| b.header.as_ref().map(|h| h.number())),
		blocks.last().and_then(|b| b.header.as_ref().map(|h| h.number())),
	) {
		(Some(first), Some(last)) if first != last => format!(" ({}..{})", first, last),
		(Some(first), Some(_)) => format!(" ({})", first),
		_ => Default::default(),
	};

	trace!(target: LOG_TARGET, "Starting import of {} blocks {}", count, blocks_range);

	let mut imported = 0;
	let mut results = vec![];
	let mut has_error = false;
	let mut blocks = blocks.into_iter();

	// Blocks in the response/drain should be in ascending order.
	loop {
		// Is there any block left to import?
		let block = match blocks.next() {
			Some(b) => b,
			None => {
				// No block left to import, success!
				return ImportManyBlocksResult { block_count: count, imported, results }
			},
		};

		let block_number = block.header.as_ref().map(|h| *h.number());
		let block_hash = block.hash;
		let import_result = if has_error {
			Err(BlockImportError::Cancelled)
		} else {
			let verification_fut = verify_single_block_metered(
				import_handle,
				blocks_origin,
				block,
				verifier,
				metrics.as_ref(),
			);
			match verification_fut.await {
				Ok(SingleBlockVerificationOutcome::Imported(import_status)) => Ok(import_status),
				Ok(SingleBlockVerificationOutcome::Verified(import_parameters)) => {
					// The actual import.
					import_single_block_metered(import_handle, import_parameters, metrics.as_ref())
						.await
				},
				Err(e) => Err(e),
			}
		};

		if let Some(metrics) = metrics.as_ref() {
			metrics.report_import::<B>(&import_result);
		}

		if import_result.is_ok() {
			trace!(
				target: LOG_TARGET,
				"Block imported successfully {:?} ({})",
				block_number,
				block_hash,
			);
			imported += 1;
		} else {
			has_error = true;
		}

		results.push((import_result, block_hash));

		Yield::new().await
	}
}

/// A future that will always `yield` on the first call of `poll` but schedules the
/// current task for re-execution.
///
/// This is done by getting the waker and calling `wake_by_ref` followed by returning
/// `Pending`. The next time the `poll` is called, it will return `Ready`.
struct Yield(bool);

impl Yield {
	fn new() -> Self {
		Self(false)
	}
}

impl Future for Yield {
	type Output = ();

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
		if !self.0 {
			self.0 = true;
			cx.waker().wake_by_ref();
			Poll::Pending
		} else {
			Poll::Ready(())
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		block_import::{
			BlockCheckParams, BlockImport, BlockImportParams, ImportResult, JustificationImport,
		},
		import_queue::Verifier,
	};
	use futures::{executor::block_on, Future};
	use parking_lot::Mutex;
	use sp_test_primitives::{Block, BlockNumber, Hash, Header};

	#[async_trait::async_trait]
	impl Verifier<Block> for () {
		async fn verify(
			&self,
			block: BlockImportParams<Block>,
		) -> Result<BlockImportParams<Block>, String> {
			Ok(BlockImportParams::new(block.origin, block.header))
		}
	}

	#[async_trait::async_trait]
	impl BlockImport<Block> for () {
		type Error = sp_consensus::Error;

		async fn check_block(
			&self,
			_block: BlockCheckParams<Block>,
		) -> Result<ImportResult, Self::Error> {
			Ok(ImportResult::imported(false))
		}

		async fn import_block(
			&self,
			_block: BlockImportParams<Block>,
		) -> Result<ImportResult, Self::Error> {
			Ok(ImportResult::imported(true))
		}
	}

	#[async_trait::async_trait]
	impl JustificationImport<Block> for () {
		type Error = sp_consensus::Error;

		async fn on_start(&mut self) -> Vec<(Hash, BlockNumber)> {
			Vec::new()
		}

		async fn import_justification(
			&mut self,
			_hash: Hash,
			_number: BlockNumber,
			_justification: Justification,
		) -> Result<(), Self::Error> {
			Ok(())
		}
	}

	#[derive(Debug, PartialEq)]
	enum Event {
		JustificationImported(Hash),
		BlockImported(Hash),
	}

	#[derive(Default)]
	struct TestLink {
		events: Mutex<Vec<Event>>,
	}

	impl Link<Block> for TestLink {
		fn blocks_processed(
			&self,
			_imported: usize,
			_count: usize,
			results: Vec<(Result<BlockImportStatus<BlockNumber>, BlockImportError>, Hash)>,
		) {
			if let Some(hash) = results.into_iter().find_map(|(r, h)| r.ok().map(|_| h)) {
				self.events.lock().push(Event::BlockImported(hash));
			}
		}

		fn justification_imported(
			&self,
			_who: RuntimeOrigin,
			hash: &Hash,
			_number: BlockNumber,
			_import_result: JustificationImportResult,
		) {
			self.events.lock().push(Event::JustificationImported(*hash))
		}
	}

	#[test]
	fn prioritizes_finality_work_over_block_import() {
		let (result_sender, mut result_port) = buffered_link::buffered_link(100_000);

		let (worker, finality_sender, block_import_sender) =
			BlockImportWorker::new(result_sender, (), Box::new(()), Some(Box::new(())), None);
		futures::pin_mut!(worker);

		let import_block = |n| {
			let header = Header {
				parent_hash: Hash::random(),
				number: n,
				extrinsics_root: Hash::random(),
				state_root: Default::default(),
				digest: Default::default(),
			};

			let hash = header.hash();

			block_import_sender
				.unbounded_send(worker_messages::ImportBlocks(
					BlockOrigin::Own,
					vec![IncomingBlock {
						hash,
						header: Some(header),
						body: None,
						indexed_body: None,
						justifications: None,
						origin: None,
						allow_missing_state: false,
						import_existing: false,
						state: None,
						skip_execution: false,
					}],
				))
				.unwrap();

			hash
		};

		let import_justification = || {
			let hash = Hash::random();
			finality_sender
				.unbounded_send(worker_messages::ImportJustification(
					sc_network_types::PeerId::random(),
					hash,
					1,
					(*b"TEST", Vec::new()),
				))
				.unwrap();

			hash
		};

		let link = TestLink::default();

		// we send a bunch of tasks to the worker
		let block1 = import_block(1);
		let block2 = import_block(2);
		let block3 = import_block(3);
		let justification1 = import_justification();
		let justification2 = import_justification();
		let block4 = import_block(4);
		let block5 = import_block(5);
		let block6 = import_block(6);
		let justification3 = import_justification();

		// we poll the worker until we have processed 9 events
		block_on(futures::future::poll_fn(|cx| {
			while link.events.lock().len() < 9 {
				match Future::poll(Pin::new(&mut worker), cx) {
					Poll::Pending => {},
					Poll::Ready(()) => panic!("import queue worker should not conclude."),
				}

				result_port.poll_actions(cx, &link).unwrap();
			}

			Poll::Ready(())
		}));

		// all justification tasks must be done before any block import work
		assert_eq!(
			&*link.events.lock(),
			&[
				Event::JustificationImported(justification1),
				Event::JustificationImported(justification2),
				Event::JustificationImported(justification3),
				Event::BlockImported(block1),
				Event::BlockImported(block2),
				Event::BlockImported(block3),
				Event::BlockImported(block4),
				Event::BlockImported(block5),
				Event::BlockImported(block6),
			]
		);
	}
}
