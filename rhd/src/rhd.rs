use std::time::{Duration, Instant};
use futures::future::poll_fn;
use tokio::timer::Delay;        
// TODO: check this
use sp_core::sr25519::{LocalizedSignature, Pair};




// TODO: We need define here at the front 
// 
// AuthorityId
// Digest
// Signature
// Candidate
// Hash



/// Justification for some state at a given round.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct UncheckedJustification {
	/// The round.
	pub round_number: u32,
	/// The digest prepared for.
	pub digest: Digest,
	/// Signatures for the prepare messages.
	pub signatures: Vec<Signature>,
}

impl UncheckedJustification {
	/// Fails if there are duplicate signatures or invalid.
	///
	/// Provide a closure for checking whether the signature is valid on a
	/// digest.
	///
	/// The closure should returns a checked justification iff the round number, digest, and signature
	/// represent a valid message and the signer was authorized to issue
	/// it.
	///
	/// The `check_message` closure may vary based on context.
	pub fn check<F>(self, threshold: usize, mut check_message: F)
		-> Result<Justification, Self>
		where
			F: FnMut(u32, &Digest, &Signature) -> Option<AuthorityId>,
	{
		let checks_out = {
			let mut checks_out = || {
				let mut voted = HashSet::new();

				for signature in &self.signatures {
					match check_message(self.round_number, &self.digest, signature) {
						None => return false,
						Some(v) => {
							if !voted.insert(v) {
								return false;
							}
						}
					}
				}

				voted.len() >= threshold
			};

			checks_out()
		};

		if checks_out {
			Ok(Justification(self))
		} else {
			Err(self)
		}
	}
}

/// A checked justification.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct Justification(UncheckedJustification);

impl Justification {
	/// Convert this justification back to unchecked.
	pub fn uncheck(self) -> UncheckedJustification {
		self.0
	}
}

impl ::std::ops::Deref for Justification {
	type Target = UncheckedJustification;

	fn deref(&self) -> &Self::Target {
		&self.0
	}
}

pub type PrepareJustification = Justification;

/// The round's state, based on imported messages.
#[derive(PartialEq, Eq, Debug, Encode, Decode)]
pub enum State {
	/// No proposal yet.
	Begin,
	/// Proposal received.
	Proposed(Candidate),
	/// Seen n - f prepares for this digest.
	Prepared(PrepareJustification),
	/// Seen n - f commits for a digest.
	Committed(Justification),
	/// Seen n - f round-advancement messages.
	Advanced(Option<PrepareJustification>),
}

#[derive(Debug, Default, Encode, Decode)]
struct VoteCounts {
	prepared: u64,
	committed: u64,
}

#[derive(Debug, Encode, Decode)]
struct Proposal {
	proposal: Candidate,
	digest: Digest,
	digest_signature: Signature,
}

/// Misbehavior which can occur.
#[derive(Debug, Clone, Encode, Decode)]
pub enum Misbehavior {
	/// Proposed out-of-turn.
	ProposeOutOfTurn(u32, Digest, Signature),
	/// Issued two conflicting proposals.
	DoublePropose(u32, (Digest, Signature), (Digest, Signature)),
	/// Issued two conflicting prepare messages.
	DoublePrepare(u32, (Digest, Signature), (Digest, Signature)),
	/// Issued two conflicting commit messages.
	DoubleCommit(u32, (Digest, Signature), (Digest, Signature)),
}


/// Accumulates messages for a given round of BFT consensus.
///
/// This isn't tied to the "view" of a single authority. It
/// keeps accurate track of the state of the BFT consensus based
/// on all messages imported.
#[derive(Debug)]
pub struct Accumulator {
	/// The round this accumulator is currently on
	pub round_number: u32,
	/// Threshold of prepare messages required to make progress
	pub threshold: usize,
	/// Current proposer/authority for this round
	pub round_proposer: AuthorityId,

	proposal: Option<Proposal>,
	prepares: HashMap<AuthorityId, (Digest, Signature)>,
	commits: HashMap<AuthorityId, (Digest, Signature)>,
	vote_counts: HashMap<Digest, VoteCounts>,
	advance_round: HashSet<AuthorityId>,
	state: State,
}

impl Accumulator {
	/// Create a new state accumulator.
	pub fn new(round_number: u32, threshold: usize, round_proposer: AuthorityId) -> Self {
		Accumulator {
			round_number,
			threshold,
			round_proposer,
			proposal: None,
			prepares: HashMap::new(),
			commits: HashMap::new(),
			vote_counts: HashMap::new(),
			advance_round: HashSet::new(),
			state: State::Begin,
		}
	}

	/// How advance votes we have seen.
	pub fn advance_votes(&self) -> usize {
		self.advance_round.len()
	}

	/// Get the round number.
	pub fn round_number(&self) -> u32 {
		self.round_number.clone()
	}

	/// Get the round proposer.
	pub fn round_proposer(&self) -> AuthorityId {
		self.round_proposer.clone()
	}

	pub fn proposal(&self) -> Option<&Candidate> {
		self.proposal.as_ref().map(|p| &p.proposal)
	}

	/// Returns a HashSet of AuthorityIds we've seen participating at any step in this round
	pub fn participants(&self) -> HashSet<&AuthorityId> {
		let mut participants = self.prepares.keys()
			.chain(self.commits.keys())
			.chain(self.advance_round.iter())
			.collect::<HashSet<&AuthorityId>>();

		if self.proposal.is_some() {
			// we only accepted the proposals, if they were made by the proposer
			participants.insert(&self.round_proposer);
		}

		participants
	}

	/// Returns a HashSet of AuthorityIds we've seen voting at any step in this round.
	/// Does not include those who we've only seen broadcast `AdvanceRound`.
	pub fn voters(&self) -> HashSet<&AuthorityId> {
		let mut participants = self.prepares.keys()
			.chain(self.commits.keys())
			.collect::<HashSet<&AuthorityId>>();

		if self.proposal.is_some() {
			// we only accepted the proposals, if they were made by the proposer
			participants.insert(&self.round_proposer);
		}

		participants
	}

	/// Inspect the current consensus state.
	pub fn state(&self) -> &State {
		&self.state
	}

	/// Import a message. Importing duplicates is fine, but the signature
	/// and authorization should have already been checked.
	pub fn import_message(
		&mut self,
		message: LocalizedMessage,
	) -> Result<(), Misbehavior> {
		// message from different round.
		if message.round_number() != self.round_number {
			return Ok(());
		}

		match message {
			LocalizedMessage::Propose(proposal) => self.import_proposal(proposal),
			LocalizedMessage::Vote(vote) => {
				let (sender, signature) = (vote.sender, vote.signature);
				match vote.vote {
					Vote::Prepare(_, d) => self.import_prepare(d, sender, signature),
					Vote::Commit(_, d) => self.import_commit(d, sender, signature),
					Vote::AdvanceRound(_) => self.import_advance_round(sender),
				}
			}
		}
	}

	fn import_proposal(
		&mut self,
		proposal: LocalizedProposal,
	) -> Result<(), Misbehavior> {
		let sender = proposal.sender;

		if sender != self.round_proposer {
			return Err(Misbehavior::ProposeOutOfTurn(
				self.round_number,
				proposal.digest,
				proposal.digest_signature)
			);
		}

		match self.proposal {
			Some(ref p) if &p.digest != &proposal.digest => {
				return Err(Misbehavior::DoublePropose(
					self.round_number,
					{
						let old = self.proposal.as_ref().expect("just checked to be Some; qed");
						(old.digest.clone(), old.digest_signature.clone())
					},
					(proposal.digest.clone(), proposal.digest_signature.clone())
				))
			}
			_ => {},
		}

		debug!(target: "bft", "Importing proposal for round {}", self.round_number);

		self.proposal = Some(Proposal {
			proposal: proposal.proposal.clone(),
			digest: proposal.digest,
			digest_signature: proposal.digest_signature,
		});

		if let State::Begin = self.state {
			self.state = State::Proposed(proposal.proposal);
		}

		Ok(())
	}

	fn import_prepare(
		&mut self,
		digest: Digest,
		sender: AuthorityId,
		signature: Signature,
	) -> Result<(), Misbehavior> {
		// ignore any subsequent prepares by the same sender.
		let threshold_prepared = match self.prepares.entry(sender.clone()) {
			Entry::Vacant(vacant) => {
				vacant.insert((digest.clone(), signature));
				let count = self.vote_counts.entry(digest.clone()).or_insert_with(Default::default);
				count.prepared += 1;

				if count.prepared >= self.threshold as u64 {
					Some(digest)
				} else {
					None
				}
			}
			Entry::Occupied(occupied) => {
				// if digest is different, that's misbehavior.
				if occupied.get().0 != digest {
					return Err(Misbehavior::DoublePrepare(
						self.round_number,
						occupied.get().clone(),
						(digest, signature)
					));
				}

				None
			}
		};

		// only allow transition to prepare from begin or proposed state.
		let valid_transition = match self.state {
			State::Begin | State::Proposed(_) => true,
			_ => false,
		};

		if let (true, Some(threshold_prepared)) = (valid_transition, threshold_prepared) {
			let signatures = self.prepares
				.values()
				.filter(|&&(ref d, _)| d == &threshold_prepared)
				.map(|&(_, ref s)| s.clone())
				.collect();

			trace!(target: "bft", "observed threshold-prepare for round {}", self.round_number);
			self.state = State::Prepared(Justification(UncheckedJustification {
				round_number: self.round_number,
				digest: threshold_prepared,
				signatures: signatures,
			}));
		}

		Ok(())
	}

	fn import_commit(
		&mut self,
		digest: Digest,
		sender: AuthorityId,
		signature: Signature,
	) -> Result<(), Misbehavior> {
		// ignore any subsequent commits by the same sender.
		let threshold_committed = match self.commits.entry(sender.clone()) {
			Entry::Vacant(vacant) => {
				vacant.insert((digest.clone(), signature));
				let count = self.vote_counts.entry(digest.clone()).or_insert_with(Default::default);
				count.committed += 1;

				if count.committed >= self.threshold as u64 {
					Some(digest)
				} else {
					None
				}
			}
			Entry::Occupied(occupied) => {
				// if digest is different, that's misbehavior.
				if occupied.get().0 != digest {
					return Err(Misbehavior::DoubleCommit(
						self.round_number,
						occupied.get().clone(),
						(digest, signature)
					));
				}

				None
			}
		};

		// transition to concluded state always valid.
		// only weird case is if the prior state was "advanced",
		// but technically it's the same behavior as if the order of receiving
		// the last "advance round" and "commit" messages were reversed.
		if let Some(threshold_committed) = threshold_committed {
			let signatures = self.commits
				.values()
				.filter(|&&(ref d, _)| d == &threshold_committed)
				.map(|&(_, ref s)| s.clone())
				.collect();

			trace!(target: "bft", "observed threshold-commit for round {}", self.round_number);
			self.state = State::Committed(Justification(UncheckedJustification {
				round_number: self.round_number,
				digest: threshold_committed,
				signatures: signatures,
			}));
		}

		Ok(())
	}

	fn import_advance_round(
		&mut self,
		sender: AuthorityId,
	) -> Result<(), Misbehavior> {
		self.advance_round.insert(sender);

		if self.advance_round.len() < self.threshold { return Ok(()) }
		trace!(target: "bft", "Witnessed threshold advance-round messages for round {}", self.round_number);

		// allow transition to new round only if we haven't produced a justification
		// yet.
		self.state = match ::std::mem::replace(&mut self.state, State::Begin) {
			State::Committed(j) => State::Committed(j),
			State::Prepared(j) => State::Advanced(Some(j)),
			State::Advanced(j) => State::Advanced(j),
			State::Begin | State::Proposed(_) => State::Advanced(None),
		};

		Ok(())
	}
}



/// Votes during a round.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum Vote {
	/// Prepare to vote for proposal with digest D.
	Prepare(u32, Digest),
	/// Commit to proposal with digest D..
	Commit(u32, Digest),
	/// Propose advancement to a new round.
	AdvanceRound(u32),
}

impl Vote {
	/// Extract the round number.
	pub fn round_number(&self) -> u32 {
		match *self {
			Vote::Prepare(round, _) => round,
			Vote::Commit(round, _) => round,
			Vote::AdvanceRound(round) => round,
		}
	}
}

/// Messages over the proposal.
/// Each message carries an associated round number.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum Message {
	/// A proposal itself.
	Propose(u32, Candidate),
	/// A vote of some kind, localized to a round number.
	Vote(Vote<Digest>),
}

impl From<Vote> for Message {
	fn from(vote: Vote) -> Self {
		Message::Vote(vote)
	}
}

/// A localized proposal message. Contains two signed pieces of data.
#[derive(Debug, Clone, Encode, Decode)]
pub struct LocalizedProposal {
	/// The round number.
	pub round_number: u32,
	/// The proposal sent.
	pub proposal: Candidate,
	/// The digest of the proposal.
	pub digest: Digest,
	/// The sender of the proposal
	pub sender: AuthorityId,
	/// The signature on the message (propose, round number, digest)
	pub digest_signature: Signature,
	/// The signature on the message (propose, round number, proposal)
	pub full_signature: Signature,
}

/// A localized vote message, including the sender.
#[derive(Debug, Clone, Encode, Decode)]
pub struct LocalizedVote {
	/// The message sent.
	pub vote: Vote<Digest>,
	/// The sender of the message
	pub sender: AuthorityId,
	/// The signature of the message.
	pub signature: Signature,
}

/// A localized message.
#[derive(Debug, Clone, Encode, Decode)]
pub enum LocalizedMessage {
	/// A proposal.
	Propose(LocalizedProposal),
	/// A vote.
	Vote(LocalizedVote),
}

impl LocalizedMessage {
	/// Extract the sender.
	pub fn sender(&self) -> &AuthorityId {
		match *self {
			LocalizedMessage::Propose(ref proposal) => &proposal.sender,
			LocalizedMessage::Vote(ref vote) => &vote.sender,
		}
	}

	/// Extract the round number.
	pub fn round_number(&self) -> u32 {
		match *self {
			LocalizedMessage::Propose(ref proposal) => proposal.round_number,
			LocalizedMessage::Vote(ref vote) => vote.vote.round_number(),
		}
	}
}

impl From<LocalizedVote> for LocalizedMessage {
	fn from(vote: LocalizedVote) -> Self {
		LocalizedMessage::Vote(vote)
	}
}

/// A reason why we are advancing round.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum AdvanceRoundReason {
	/// We received enough `AdvanceRound` messages to advance to the next round.
	Timeout,
	/// We got enough `Prepare` messages for a future round to fast-forward to it.
	WasBehind,
}

/// Communication that can occur between participants in consensus.
#[derive(Debug, Clone, Encode, Decode)]
pub enum Communication {
	/// A consensus message (proposal or vote)
	Consensus(LocalizedMessage),
	/// Auxiliary communication (just proof-of-lock for now).
	Auxiliary(PrepareJustification),
}

/// Committed successfully.
#[derive(Debug, Clone, Encode, Decode)]
pub struct Committed {
	/// The candidate committed for. This will be unknown if
	/// we never witnessed the proposal of the last round.
	pub candidate: Option<Candidate>,
	/// The round number we saw the commit in.
	pub round_number: u32,
	/// A justification for the candidate.
	pub justification: Justification,
}

struct Locked {
	justification: PrepareJustification,
}

impl Locked {
	fn digest(&self) -> &Digest {
		&self.justification.digest
	}
}

// the state of the local node during the current state of consensus.
// behavior is different when locked on a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalState {
	Start,
	Proposed,
	Prepared(bool), // whether we thought it valid.
	Committed,
	VoteAdvance,
}

#[derive(Debug)]
struct Sending<T> {
	items: VecDeque<T>,
	flushing: bool,
}

impl<T> Sending<T> {
	fn with_capacity(n: usize) -> Self {
		Sending {
			items: VecDeque::with_capacity(n),
			flushing: false,
		}
	}

	fn push(&mut self, item: T) {
		self.items.push_back(item);
	}

	// process all the sends into the sink.
	fn process_all<S: Sink<SinkItem=T>>(&mut self, sink: &mut S) -> Poll<(), S::SinkError> {
		loop {
			while let Some(item) = self.items.pop_front() {
				match sink.start_send(item) {
					Err(e) => return Err(e),
					Ok(AsyncSink::NotReady(item)) => {
						self.items.push_front(item);
						break;
					}
					Ok(AsyncSink::Ready) => {
						// At least one item is buffered into the sink so we must ensure
						// that at some point we will call `poll_complete`.
						self.flushing = true;
					}
				}
			}

			if self.flushing {
				if let Async::Ready(()) = sink.poll_complete()? {
					self.flushing = false;
				}
			}

			match (self.flushing, self.items.len()) {
				// Still flushing, schedule to poll later.
				(true, _) => return Ok(Async::NotReady),
				// Return `Ready` only if all items have been sent and flushed.
				(false, pending) if pending == 0 => return Ok(Async::Ready(())),
				// Flushing is complete, however there are still pending items left.
				(false, _) => continue,
			}
		}
	}
}

/// Instance of Rhd engine context
struct Context {
	key: Arc<Pair>,
	parent_hash: Hash,
    authorities: Vec<AuthorityId>,
    rhd_worker: Arc<&mut RhdWorker>,
}

impl Context {
	/// Get the local authority ID.
	fn local_id(&self) -> AuthorityId {
	    self.key.public().into()
    }

	/// Get the digest of a candidate.
	fn candidate_digest(&self, candidate: &Candidate) -> Digest {
        candidate.hash()
    }

	/// Sign a message using the local authority ID.
	/// In the case of a proposal message, it should sign on the hash and
	/// the bytes of the proposal.
	fn sign_local(&self, message: Message) -> LocalizedMessage {
		sign_message(&*self.key, self.parent_hash.clone(), message)
    }

	/// Get the proposer for a given round of consensus.
	fn round_proposer(&self, round: u32) -> AuthorityId {
        let len = self.authorities.len();
		let offset = round % len;
		let proposer = self.authorities[offset as usize].clone();
		trace!(target: "rhd", "proposer for round {} is {}", round, proposer);

		proposer
    }

	/// This hook is called when we advance from current `round` to `next_round`. `proposal` is
	/// `Some` if there was one on the current `round`.
	fn on_advance_round(
		&self, 
		accumulator: &Accumulator,
		round: u32, 
		next_round: u32,
		reason: AdvanceRoundReason,
	) {
		let _ = (accumulator, round, next_round, reason);

        // TODO
	}

	/// Get the best proposal.
	fn proposal(&self) -> impl Future<Item=Candidate, Error=()> {
        //let ask_proposal_msg = ...;
        self.rhd_worker.ap_tx.unbounded_send( ask_proposal_msg );

        poll_fn(move || -> Poll<Candidate, ()> {
            match self.rhd_worker.gp_rx.poll()? {
                Async::Ready(Some(proposal)) => {
                    Ok(Async::Ready(proposal))
                }
                _ => Ok(Async::NotReady)
            }
        })
    }
	/// Whether the proposal is valid.
	fn proposal_valid(&self, proposal: Candidate) -> impl Future<Item=bool, Error=()> {
        // now, we think it's valid and be ready 
        poll_fn(move || -> Poll<bool, ()> {
            Ok(Async::Ready(true))
        })
    }

	/// Create a round timeout. The context will determine the correct timeout
	/// length, and create a future that will resolve when the timeout is
	/// concluded.
	fn begin_round_timeout(&self, round: u32) -> impl Future<Item=(), Error=()> {
        // We give timeout 10 seconds for test
        let timeout = Duration::new(10, 0);
        let fut = Delay::new(Instant::now() + timeout)
            .map(|_|())
            .map_err(|_|());

        Box::new(fut)
    }

}


// This structure manages a single "view" of consensus.
//
// We maintain two message accumulators: one for the round we are currently in,
// and one for a future round.
//
// We advance the round accumulators when one of two conditions is met:
//   - we witness consensus of advancement in the current round. in this case we
//     advance by one.
//   - a higher threshold-prepare is broadcast to us. in this case we can
//     advance to the round of the threshold-prepare. this is an indication
//     that we have experienced severe asynchrony/clock drift with the remainder
//     of the other authorities, and it is unlikely that we can assist in
//     consensus meaningfully. nevertheless we make an attempt.
struct Strategy {
	nodes: usize,
	max_faulty: usize,
	local_state: LocalState,
	locked: Option<Locked>,
	notable_candidates: HashMap<Digest, Candidate>,
	current_accumulator: Accumulator,
	future_accumulators: BTreeMap<u32, Accumulator>,
	local_id: AuthorityId,
	misbehavior: HashMap<AuthorityId, Misbehavior>,
	earliest_lock_round: u32,

	fetching_proposal: Option<impl Future<Item=Candidate, Error=()>>,
	evaluating_proposal: Option<impl Future<Item=bool, Error=()>>,
	round_timeout: Option<future::Fuse<impl Future<Item=(), Error=()>>>,
}

impl Strategy {
	fn create(context: &Context, nodes: usize, max_faulty: usize) -> Self {
		let threshold = bft_threshold(nodes, max_faulty);

		let current_accumulator = Accumulator::new(
			0,
			threshold,
			context.round_proposer(0),
		);

		Strategy {
			nodes,
			max_faulty,
			current_accumulator,
			future_accumulators: BTreeMap::new(),
			local_state: LocalState::Start,
			locked: None,
			notable_candidates: HashMap::new(),
			local_id: context.local_id(),
			misbehavior: HashMap::new(),
			earliest_lock_round: 0,

			fetching_proposal: None,
			evaluating_proposal: None,
			round_timeout: None,
		}
	}

	fn current_round(&self) -> u32 {
		self.current_accumulator.round_number()
	}

	fn import_message(
		&mut self,
		context: &Context,
		msg: LocalizedMessage
	) {
		let round_number = msg.round_number();

		let sender = msg.sender().clone();
		let current_round = self.current_round();
		let misbehavior = if round_number == current_round {
			self.current_accumulator.import_message(msg)
		} else if round_number > current_round {
			let threshold = bft_threshold(self.nodes, self.max_faulty);

			let future_acc = self.future_accumulators.entry(round_number).or_insert_with(|| {
				Accumulator::new(
					round_number,
					threshold,
					context.round_proposer(round_number),
				)
			});

			future_acc.import_message(msg)
		} else {
			Ok(())
		};

		if let Err(misbehavior) = misbehavior {
			self.misbehavior.insert(sender, misbehavior);
		}
	}

	fn import_lock_proof(
		&mut self,
		context: &Context,
		justification: PrepareJustification,
	) {
		// TODO: find a way to avoid processing of the signatures if the sender is
		// not the primary or the round number is low.
		if justification.round_number > self.current_round() {
			// jump ahead to the prior round as this is an indication of a supermajority
			// good nodes being at least on that round.
			self.advance_to_round(context, justification.round_number, AdvanceRoundReason::WasBehind);
		}

		let lock_to_new = justification.round_number >= self.earliest_lock_round; 

		if lock_to_new {
			self.earliest_lock_round = justification.round_number;
			self.locked = Some(Locked { justification })
		}
	}

	// poll the strategy: this will queue messages to be sent and advance
	// rounds if necessary.
	//
	// only call within the context of a `Task`.
	fn poll(
		&mut self,
		context: &Context,
		sending: &mut Sending<Communication>
	)
		-> Poll<Committed, ()>
	{
		let mut last_watermark = (self.current_round(), self.local_state);

		// poll until either completion or state doesn't change.
		loop {
			trace!(target: "bft", "Polling BFT logic. State={:?}", last_watermark);
			match self.poll_once(context, sending)? {
				Async::Ready(x) => return Ok(Async::Ready(x)),
				Async::NotReady => {
					let new_watermark = (self.current_round(), self.local_state);

					if new_watermark == last_watermark {
						return Ok(Async::NotReady)
					} else {
						last_watermark = new_watermark;
					}
				}
			}
		}
	}

	// perform one round of polling: attempt to broadcast messages and change the state.
	// if the round or internal round-state changes, this should be called again.
	fn poll_once(
		&mut self,
		context: &Context,
		sending: &mut Sending<Communication>
	)
		-> Poll<Committed, ()>
	{
		self.propose(context, sending)?;
		self.prepare(context, sending)?;
		self.commit(context, sending);
		self.vote_advance(context, sending)?;

		let advance = match self.current_accumulator.state() {
			&State::Advanced(ref p_just) => {
				// lock to any witnessed prepare justification.
				if let Some(p_just) = p_just.as_ref() {
					self.locked = Some(Locked { justification: p_just.clone() });
				}

				let round_number = self.current_round();
				Some(round_number + 1)
			}
			&State::Committed(ref just) => {
				// fetch the agreed-upon candidate:
				//   - we may not have received the proposal in the first place
				//   - there is no guarantee that the proposal we got was agreed upon
				//     (can happen if faulty primary)
				//   - look in the candidates of prior rounds just in case.
				let candidate = self.current_accumulator
					.proposal()
					.and_then(|c| if context.candidate_digest(c) == just.digest {
						Some(c.clone())
					} else {
						None
					})
					.or_else(|| self.notable_candidates.get(&just.digest).cloned());

				let committed = Committed {
					candidate,
					round_number: self.current_accumulator.round_number(),
					justification: just.clone()
				};

				return Ok(Async::Ready(committed))
			}
			_ => None,
		};

		if let Some(new_round) = advance {
			self.advance_to_round(context, new_round, AdvanceRoundReason::Timeout);
		}

		Ok(Async::NotReady)
	}

	fn propose(
		&mut self,
		context: &Context,
		sending: &mut Sending<Communication>
	)
		-> Result<(), ()>
	{
		if let LocalState::Start = self.local_state {
			let mut propose = false;
			if let &State::Begin = self.current_accumulator.state() {
				let round_number = self.current_round();
				let primary = context.round_proposer(round_number);
				propose = self.local_id == primary;
			};

			if !propose { return Ok(()) }

			// obtain the proposal to broadcast.
			let proposal = match self.locked {
				Some(ref locked) => {
					// TODO: it's possible but very unlikely that we don't have the
					// corresponding proposal for what we are locked to.
					//
					// since this is an edge case on an edge case, it is fine
					// to eat the round timeout for now, but it can be optimized by
					// broadcasting an advance vote.
					self.notable_candidates.get(locked.digest()).cloned()
				}
				None => {
					let res = self.fetching_proposal
						.get_or_insert_with(|| context.proposal())
						.poll()?;

					match res {
						Async::Ready(p) => Some(p),
						Async::NotReady => None,
					}
				}
			};

			if let Some(proposal) = proposal {
				self.fetching_proposal = None;

				let message = Message::Propose(
					self.current_round(),
					proposal
				);

				self.import_and_send_message(message, context, sending);

				// broadcast the justification along with the proposal if we are locked.
				if let Some(ref locked) = self.locked {
					sending.push(
						Communication::Auxiliary(locked.justification.clone())
					);
				}

				self.local_state = LocalState::Proposed;
			}
		}

		Ok(())
	}

	fn prepare(
		&mut self,
		context: &Context,
		sending: &mut Sending<Communication>
	)
		-> Result<(), ()>
	{
		// prepare only upon start or having proposed.
		match self.local_state {
			LocalState::Start | LocalState::Proposed => {},
			_ => return Ok(())
		};

		let mut prepare_for = None;

		// we can't prepare until something was proposed.
		if let &State::Proposed(ref candidate) = self.current_accumulator.state() {
			let digest = context.candidate_digest(candidate);

			// vote to prepare only if we believe the candidate to be valid and
			// we are not locked on some other candidate.
			match &mut self.locked {
				&mut Some(ref locked) if locked.digest() != &digest => {}
				locked => {
					let res = self.evaluating_proposal
						.get_or_insert_with(|| context.proposal_valid(candidate))
						.poll()?;

					if let Async::Ready(valid) = res {
						self.evaluating_proposal = None;
						self.local_state = LocalState::Prepared(valid);

						if valid {
							prepare_for = Some(digest);
						} else {
							// if the locked block is bad, unlock from it and
							// refuse to lock to anything prior to it.
							if locked.as_ref().map_or(false, |locked| locked.digest() == &digest) {
								*locked = None;
								self.earliest_lock_round = ::std::cmp::max(
									self.current_accumulator.round_number(),
									self.earliest_lock_round,
								);
							}
						}
					}
				}
			}
		}

		if let Some(digest) = prepare_for {
			let message = Vote::Prepare(
				self.current_round(),
				digest
			).into();

			self.import_and_send_message(message, context, sending);
		}

		Ok(())
	}

	fn commit(
		&mut self,
		context: &Context,
		sending: &mut Sending<Communication>
	) {
		// commit only if we haven't voted to advance or committed already
		match self.local_state {
			LocalState::Committed | LocalState::VoteAdvance => return,
			_ => {}
		}

		let mut commit_for = None;

		let thought_good = match self.local_state {
			LocalState::Prepared(good) => good,
			_ => true, // assume true.
		};

		if let &State::Prepared(ref p_just) = self.current_accumulator.state() {
			// we are now locked to this prepare justification.
			// refuse to lock if the thing is bad.
			self.earliest_lock_round = self.current_accumulator.round_number();
			if thought_good {
				let digest = p_just.digest.clone();
				self.locked = Some(Locked { justification: p_just.clone() });
				commit_for = Some(digest);
			}
		}

		if let Some(digest) = commit_for {
			let message = Vote::Commit(
				self.current_round(),
				digest
			).into();

			self.import_and_send_message(message, context, sending);
			self.local_state = LocalState::Committed;
		}
	}

	fn vote_advance(
		&mut self,
		context: &Context,
		sending: &mut Sending<Communication>
	)
		-> Result<(), ()>
	{
		// we can vote for advancement under all circumstances unless we have already.
		if let LocalState::VoteAdvance = self.local_state { return Ok(()) }

		// if we got f + 1 advance votes, or the timeout has fired, and we haven't
		// sent an AdvanceRound message yet, do so.
		let mut attempt_advance = self.current_accumulator.advance_votes() > self.max_faulty;

		// if we evaluated the proposal and it was bad, vote to advance round.
		if let LocalState::Prepared(false) = self.local_state {
			attempt_advance = true;
		}

		// if the timeout has fired, vote to advance round.
		let round_number = self.current_accumulator.round_number();
		let timer_res = self.round_timeout
			.get_or_insert_with(|| context.begin_round_timeout(round_number).fuse())
			.poll()?;

		if let Async::Ready(_) = timer_res { attempt_advance = true }

		if attempt_advance {
			let message = Vote::AdvanceRound(
				self.current_round(),
			).into();

			self.import_and_send_message(message, context, sending);
			self.local_state = LocalState::VoteAdvance;
		}

		Ok(())
	}

	fn advance_to_round(&mut self, context: &Context, round: u32, reason: AdvanceRoundReason) {
		assert!(round > self.current_round());
		trace!(target: "bft", "advancing to round {}", round);

		self.fetching_proposal = None;
		self.evaluating_proposal = None;
		self.round_timeout = None;
		self.local_state = LocalState::Start;

		// notify the context that we are about to advance round.
		context.on_advance_round(
			&self.current_accumulator,
			self.current_round(),
			round,
			reason,
		);

		// when advancing from a round, store away the witnessed proposal.
		//
		// if we or other participants end up locked on that candidate,
		// we will have it.
		if let Some(proposal) = self.current_accumulator.proposal() {
			let digest = context.candidate_digest(proposal);
			self.notable_candidates.entry(digest).or_insert_with(|| proposal.clone());
		}

		// if we jump ahead more than one round, get rid of the ones in between.
		for irrelevant in (self.current_round() + 1)..round {
			self.future_accumulators.remove(&irrelevant);
		}

		// use stored future accumulator for given round or create if it doesn't exist.
		self.current_accumulator = match self.future_accumulators.remove(&round) {
			Some(x) => x,
			None => Accumulator::new(
				round,
				bft_threshold(self.nodes, self.max_faulty),
				context.round_proposer(round),
			),
		};
	}

	fn import_and_send_message(
		&mut self,
		message: Message,
		context: &Context,
		sending: &mut Sending<Communication>
	) {
		let signed_message = context.sign_local(message);
		self.import_message(context, signed_message.clone());
		sending.push(Communication::Consensus(signed_message));
	}
}

/// Future that resolves upon BFT agreement for a candidate.
#[must_use = "futures do nothing unless polled"]
pub struct Agreement {
    context: Arc<&mut RhdWorker>, // ???
	strategy: Strategy,
	input: UnboundedReceiver<Communication>,
	output: UnboundedSender<Communication>,
	concluded: Option<Committed>,
	sending: Sending<Communication>,
}

impl Agreement {
	/// Get a reference to the underlying context.
	pub fn context(&self) -> &context {
		&self.context
	}

	/// Drain the misbehavior vector.
	pub fn drain_misbehavior(&mut self) -> hash_map::Drain<AuthorityId, Misbehavior> {
		self.strategy.misbehavior.drain()
	}

	/// Fast-foward the round to the given number.
	pub fn fast_forward(&mut self, round: u32) {
		if round > self.strategy.current_round() {
			self.strategy.advance_to_round(&self.context, round, AdvanceRoundReason::WasBehind);
			self.strategy.earliest_lock_round = round;
		}
	}
}

impl Future for Agreement {
	type Item = Committed;
	type Error = ();

	fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
		// even if we've observed the conclusion, wait until all
		// pending outgoing messages are flushed.
		if let Some(just) = self.concluded.take() {
			return Ok(match self.sending.process_all(&mut self.output)? {
				Async::Ready(()) => Async::Ready(just),
				Async::NotReady => {
					self.concluded = Some(just);
					Async::NotReady
				}
			})
		}

		// drive state machine as long as there are new messages.
		let mut driving = true;
		while driving {
			driving = match self.input.poll()? {
                // msg here is Option<>
				Async::Ready(msg) => {
                    // TODO: change this line
					match msg.ok_or(InputStreamConcluded)? {
						Communication::Consensus(message) => self.strategy.import_message(&self.context, message),
						Communication::Auxiliary(lock_proof)
							=> self.strategy.import_lock_proof(&self.context, lock_proof),
					}

					true
				}
				Async::NotReady => false,
			};

			// drive state machine after handling new input.
			if let Async::Ready(just) = self.strategy.poll(&self.context, &mut self.sending)? {
				self.concluded = Some(just);
				return self.poll();
			}
		}

		// make progress on flushing all pending messages.
		let _ = self.sending.process_all(&mut self.output)?;
		Ok(Async::NotReady)
	}
}


/// Attempt to reach BFT agreement on a candidate.
///
/// `nodes` is the number of nodes in the system.
/// `max_faulty` is the maximum number of faulty nodes. Should be less than
/// 1/3 of `nodes`, otherwise agreement may never be reached.
///
/// The input stream should never logically conclude. The logic here assumes
/// that messages flushed to the output stream will eventually reach other nodes.
///
/// Note that it is possible to witness agreement being reached without ever
/// seeing the candidate. Any candidates seen will be checked for validity.
///
/// Although technically the agreement will always complete (given the eventual
/// delivery of messages), in practice it is possible for this future to
/// conclude without having witnessed the conclusion.
/// In general, this future should be pre-empted by the import of a justification
/// set for this block height.
pub fn agree(context: Context, nodes: usize, max_faulty: usize, input: UnboundedReceiver<Communication>, output: UnboundedSender<Communication>) -> Agreement
{
	let strategy = Strategy::create(&context, nodes, max_faulty);
	Agreement {
		context,
		strategy,
		input,
		output,
		concluded: None,
		sending: Sending::with_capacity(4),
	}
}

// =================== Helper ======================

// get the "full BFT" threshold based on an amount of nodes and
// a maximum faulty. if nodes == 3f + 1, then threshold == 2f + 1.
fn bft_threshold(nodes: usize, max_faulty: usize) -> usize {
	nodes - max_faulty
}

// actions in the signature scheme.
#[derive(Encode)]
enum Action {
	Prepare(u32, Hash),
	Commit(u32, Hash),
	AdvanceRound(u32),
	// signatures of header hash and full candidate are both included.
	ProposeHeader(u32, Hash),
	Propose(u32, Candidate),
}

/// Sign a BFT message with the given key.
pub fn sign_message(
	key: &Pair,
	parent_hash: Hash
	message: Message,
) -> LocalizedMessage {
	let signer = key.public();

	let sign_action = |action: Action| {
		let to_sign = localized_encode(parent_hash.clone(), action);

		LocalizedSignature {
			signer: signer.clone(),
			signature: key.sign(&to_sign),
		}
	};

	match message {
		Message::Propose(r, proposal) => {
			let header_hash = proposal.hash();
			let action_header = Action::ProposeHeader(r as u32, header_hash.clone());
			let action_propose = Action::Propose(r as u32, proposal.clone());

			LocalizedMessage::Propose(LocalizedProposal {
				round_number: r,
				proposal,
				digest: header_hash,
				sender: signer.clone().into(),
				digest_signature: sign_action(action_header),
				full_signature: sign_action(action_propose),
			})
		}
		Message::Vote(vote) => LocalizedMessage::Vote({
			let action = match vote {
				Vote::Prepare(r, h) => Action::Prepare(r as u32, h),
				Vote::Commit(r, h) => Action::Commit(r as u32, h),
			    Vote::AdvanceRound(r) => Action::AdvanceRound(r as u32),
			};

			LocalizedVote {
				vote: vote,
				sender: signer.clone().into(),
				signature: sign_action(action),
			}
		})
	}
}

// encode something in a way which is localized to a specific parent-hash
fn localized_encode(parent_hash: Hash, value: Action) -> Vec<u8> {
	(parent_hash, value).encode()
}



