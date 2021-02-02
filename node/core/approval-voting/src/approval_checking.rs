// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Utilities for checking whether a candidate has been approved under a given block.

use polkadot_node_primitives::approval::DelayTranche;
use bitvec::slice::BitSlice;
use bitvec::order::Lsb0 as BitOrderLsb0;

use crate::persisted_entries::{ApprovalEntry, CandidateEntry};
use crate::time::Tick;

/// The required tranches of assignments needed to determine whether a candidate is approved.
#[derive(Debug, PartialEq)]
pub enum RequiredTranches {
	/// All validators appear to be required, based on tranches already taken and remaining
	/// no-shows.
	All,
	/// More tranches required - We're awaiting more assignments. The given `DelayTranche`
	/// indicates the upper bound of tranches that should broadcast based on the last no-show.
	Pending(DelayTranche),
	/// An exact number of required tranches and a number of no-shows. This indicates that
	/// the amount of `needed_approvals` are assigned and additionally all no-shows are
	/// covered.
	Exact(DelayTranche, usize),
}

/// Check the approval of a candidate.
pub fn check_approval(
	candidate: &CandidateEntry,
	approval: &ApprovalEntry,
	required: RequiredTranches,
) -> bool {
	match required {
		RequiredTranches::Pending(_) => false,
		RequiredTranches::All => {
			let approvals = candidate.approvals();
			3 * approvals.count_ones() > 2 * approvals.len()
		}
		RequiredTranches::Exact(tranche, no_shows) => {
			// whether all assigned validators up to tranche less no_shows have approved.
			// e.g. if we had 5 tranches and 1 no-show, we would accept all validators in
			// tranches 0..=5 except for 1 approving. In that example, we also accept all
			// validators in tranches 0..=5 approving, but that would indicate that the
			// RequiredTranches value was incorrectly constructed, so it is not realistic.
			// If there are more missing approvals than there are no-shows, that indicates
			// that there are some assignments which are not yet no-shows, but may become
			// no-shows.

			let mut assigned_mask = approval.assignments_up_to(tranche);
			let approvals = candidate.approvals();

			let n_assigned = assigned_mask.count_ones();

			// Filter the amount of assigned validators by those which have approved.
			assigned_mask &= approvals.iter().by_val();
			let n_approved = assigned_mask.count_ones();

			// note: the process of computing `required` only chooses `exact` if
			// that will surpass a minimum amount of checks.
			// shouldn't typically go above, since all no-shows are supposed to be covered.
			n_approved + no_shows >= n_assigned
		}
	}
}

/// Determine the amount of tranches of assignments needed to determine approval of a candidate.
pub fn tranches_to_approve(
	approval_entry: &ApprovalEntry,
	approvals: &BitSlice<BitOrderLsb0, u8>,
	tranche_now: DelayTranche,
	block_tick: Tick,
	no_show_duration: Tick,
	needed_approvals: usize,
) -> RequiredTranches {
	// This function progresses through a series of states while looping over the tranches
	// that we are aware of. First, we perform an initial count of the number of assignments
	// until we reach the number of needed assignments for approval. As we progress, we count the
	// number of no-shows in each tranche.
	//
	// Then, if there are any no-shows, we proceed into a series of subsequent states for covering
	// no-shows.
	//
	// We cover each no-show by a non-empty tranche, keeping track of the amount of further
	// no-shows encountered along the way. Once all of the no-shows we were previously aware
	// of are covered, we then progress to cover the no-shows we encountered while covering those,
	// and so on.
	#[derive(Debug)]
	enum State {
		// (assignments, no-shows)
		InitialCount(usize, usize),
		// (assignments, covered no-shows, covering no-shows, uncovered no-shows),
		CoverNoShows(usize, usize, usize, usize),
	}

	impl State {
		fn output(
			&self,
			tranche: DelayTranche,
			tranche_now: DelayTranche,
			needed_approvals: usize,
			n_validators: usize,
		) -> RequiredTranches {
			match *self {
				State::InitialCount(assignments, no_shows) =>
					if assignments >= needed_approvals && no_shows == 0 {
						RequiredTranches::Exact(tranche, 0)
					} else {
						// This happens only if there are not enough assignments, period.
						//
						// However, within the context of this function in particular,
						// it can happen if there are enough assignments but there are no-shows.
						// The calling code would have already transitioned the state to
						// `CoverNoShows` before invoking this function, so that is not possible
						// in practice.
						//
						// In the case of not-enough assignments, we want assignments to broadcast based
						// on timer, so we treat it as though there are no uncovered no-shows.
						if no_shows == 0 {
							RequiredTranches::Pending(tranche_now)
						} else if assignments < needed_approvals {
							// This branch is only hit if there are no-shows before there
							// are enough initial assignments.
							RequiredTranches::Pending(tranche_now)
						} else {
							// This branch is never hit, as explained above.
							RequiredTranches::Pending(tranche + no_shows as DelayTranche)
						}
					},
				State::CoverNoShows(total_assignments, covered, covering, uncovered) =>
					if covering == 0 && uncovered == 0 {
						RequiredTranches::Exact(tranche, covered)
					} else if total_assignments + covering + uncovered >= n_validators  {
						RequiredTranches::All
					} else {
						RequiredTranches::Pending(tranche + (covering + uncovered) as DelayTranche)
					},
			}
		}
	}

	let tick_now = tranche_now as Tick + block_tick;
	let n_validators = approval_entry.n_validators();

	approval_entry.tranches().iter()
		.take_while(|t| t.tranche() <= tranche_now)
		.scan(Some(State::InitialCount(0, 0)), |state, tranche| {
			// The `Option` here is used for early exit.
			let s = match state.take() {
				None => return None,
				Some(s) => s,
			};

			let n_assignments = tranche.assignments().len();

			// count no-shows. An assignment is a no-show if there is no corresponding approval vote
			// after a fixed duration.
			let no_shows = tranche.assignments().iter().filter(|(v_index, tick)| {
				tick + no_show_duration <= tick_now
					&& approvals.get(*v_index as usize).map(|b| !*b).unwrap_or(true)
			}).count();

			let s = match s {
				State::InitialCount(total_assignments, no_shows_so_far) => {
					let no_shows = no_shows + no_shows_so_far;
					let total_assignments = total_assignments + n_assignments;
					if dbg!(total_assignments) >= needed_approvals {
						if no_shows == 0 {
							// Note that this state will never be advanced
							// as we will return `RequiredTranches::Exact`.
							State::InitialCount(total_assignments, 0)
						} else {
							// We reached our desired assignment count, but had no-shows.
							// Begin covering them.
							State::CoverNoShows(total_assignments, 0, no_shows, 0)
						}
					} else {
						// Keep counting
						State::InitialCount(total_assignments, no_shows)
					}
				}
				State::CoverNoShows(total_assignments, covered, covering, uncovered) => {
					let uncovered = no_shows + uncovered;
					let total_assignments = total_assignments + n_assignments;

					if n_assignments == 0 {
						// no-shows are only covered by non-empty tranches.
						State::CoverNoShows(total_assignments, covered, covering, uncovered)
					} else if covering == 1 {
						// Progress onto another round of covering uncovered no-shows.
						// Note that if `uncovered` is 0, this state will never be advanced
						// as we will return `RequiredTranches::Exact`.
						State::CoverNoShows(total_assignments, covered + 1, uncovered, 0)
					} else {
						// we covered one no-show with a non-empty tranche. continue doing so.
						State::CoverNoShows(total_assignments, covered + 1, covering - 1, uncovered)
					}
				}
			};

			let output = s.output(tranche.tranche(), tranche_now, needed_approvals, n_validators);
			match output {
				RequiredTranches::Exact(_, _) | RequiredTranches::All => {
					// Wipe the state clean so the next iteration of this closure will terminate
					// the iterator. This guarantees that we can call `last` further down to see
					// either a `Finished` or `Pending` result
					*state = None;
				}
				RequiredTranches::Pending(_) => {
					// Pending results are only interesting when they are the last result of the iterator
					// i.e. we never achieve a satisfactory level of assignment.
					*state = Some(s);
				}
			}

			Some(output)
		})
		.last()
		// The iterator is empty only when we are aware of no assignments up to the current tranche.
		// Any assignments up to now should be broadcast. Typically this will happen when
		// `tranche_now == 0`.
		.unwrap_or(RequiredTranches::Pending(tranche_now))
}

#[cfg(test)]
mod tests {
	use super::*;

	use polkadot_primitives::v1::GroupIndex;
	use bitvec::bitvec;
	use bitvec::order::Lsb0 as BitOrderLsb0;

	use crate::approval_db;

	#[test]
	fn pending_is_not_approved() {
		let candidate = approval_db::v1::CandidateEntry {
			candidate: Default::default(),
			session: 0,
			block_assignments: Default::default(),
			approvals: Default::default(),
		}.into();

		let approval_entry = approval_db::v1::ApprovalEntry {
			tranches: Vec::new(),
			assignments: Default::default(),
			our_assignment: None,
			backing_group: GroupIndex(0),
			approved: false,
		}.into();

		assert!(!check_approval(&candidate, &approval_entry, RequiredTranches::Pending(0)));
	}

	#[test]
	fn all_requires_supermajority() {
		let mut candidate: CandidateEntry = approval_db::v1::CandidateEntry {
			candidate: Default::default(),
			session: 0,
			block_assignments: Default::default(),
			approvals: bitvec![BitOrderLsb0, u8; 0; 10],
		}.into();

		for i in 0..6 {
			candidate.mark_approval(i);
		}

		let approval_entry = approval_db::v1::ApprovalEntry {
			tranches: Vec::new(),
			assignments: bitvec![BitOrderLsb0, u8; 1; 10],
			our_assignment: None,
			backing_group: GroupIndex(0),
			approved: false,
		}.into();

		assert!(!check_approval(&candidate, &approval_entry, RequiredTranches::All));

		candidate.mark_approval(6);
		assert!(check_approval(&candidate, &approval_entry, RequiredTranches::All));
	}

	#[test]
	fn exact_takes_only_assignments_up_to() {
		let mut candidate: CandidateEntry = approval_db::v1::CandidateEntry {
			candidate: Default::default(),
			session: 0,
			block_assignments: Default::default(),
			approvals: bitvec![BitOrderLsb0, u8; 0; 10],
		}.into();

		for i in 0..6 {
			candidate.mark_approval(i);
		}

		let approval_entry = approval_db::v1::ApprovalEntry {
			tranches: vec![
				approval_db::v1::TrancheEntry {
					tranche: 0,
					assignments: (0..4).map(|i| (i, 0.into())).collect(),
				},
				approval_db::v1::TrancheEntry {
					tranche: 1,
					assignments: (4..6).map(|i| (i, 1.into())).collect(),
				},
				approval_db::v1::TrancheEntry {
					tranche: 2,
					assignments: (6..10).map(|i| (i, 0.into())).collect(),
				},
			],
			assignments: bitvec![BitOrderLsb0, u8; 1; 10],
			our_assignment: None,
			backing_group: GroupIndex(0),
			approved: false,
		}.into();

		assert!(check_approval(&candidate, &approval_entry, RequiredTranches::Exact(1, 0)));
		assert!(!check_approval(&candidate, &approval_entry, RequiredTranches::Exact(2, 0)));
		assert!(check_approval(&candidate, &approval_entry, RequiredTranches::Exact(2, 4)));
	}

	#[test]
	fn tranches_to_approve_everyone_present() {
		let block_tick = 0;
		let no_show_duration = 10;
		let needed_approvals = 4;

		let mut approval_entry: ApprovalEntry = approval_db::v1::ApprovalEntry {
			tranches: Vec::new(),
			assignments: bitvec![BitOrderLsb0, u8; 0; 4],
			our_assignment: None,
			backing_group: GroupIndex(0),
			approved: false,
		}.into();

		approval_entry.import_assignment(0, 0, block_tick);
		approval_entry.import_assignment(0, 1, block_tick);

		approval_entry.import_assignment(1, 2, block_tick + 1);
		approval_entry.import_assignment(1, 3, block_tick + 1);

		let approvals = bitvec![BitOrderLsb0, u8; 1; 4];

		assert_eq!(
			tranches_to_approve(
				&approval_entry,
				&approvals,
				2,
				block_tick,
				no_show_duration,
				needed_approvals,
			),
			RequiredTranches::Exact(1, 0),
		);
	}

	#[test]
	fn tranches_to_approve_no_shows_before_initial_count() {
		let block_tick = 20;
		let no_show_duration = 10;
		let needed_approvals = 4;

		let mut approval_entry: ApprovalEntry = approval_db::v1::ApprovalEntry {
			tranches: Vec::new(),
			assignments: bitvec![BitOrderLsb0, u8; 0; 4],
			our_assignment: None,
			backing_group: GroupIndex(0),
			approved: false,
		}.into();

		approval_entry.import_assignment(0, 0, block_tick);
		approval_entry.import_assignment(0, 1, block_tick);

		approval_entry.import_assignment(1, 2, block_tick);

		let mut approvals = bitvec![BitOrderLsb0, u8; 0; 4];
		approvals.set(0, true);
		approvals.set(1, true);

		let tranche_now = no_show_duration as DelayTranche + 1;
		assert_eq!(
			tranches_to_approve(
				&approval_entry,
				&approvals,
				tranche_now,
				block_tick,
				no_show_duration,
				needed_approvals,
			),
			RequiredTranches::Pending(tranche_now),
		);
	}

	#[test]
	fn tranches_to_approve_not_enough_initial() {
		let block_tick = 0;
		let no_show_duration = 10;
		let needed_approvals = 4;

		let mut approval_entry: ApprovalEntry = approval_db::v1::ApprovalEntry {
			tranches: Vec::new(),
			assignments: bitvec![BitOrderLsb0, u8; 0; 4],
			our_assignment: None,
			backing_group: GroupIndex(0),
			approved: false,
		}.into();

		approval_entry.import_assignment(0, 0, block_tick);
		approval_entry.import_assignment(0, 1, block_tick);

		approval_entry.import_assignment(1, 3, block_tick + 1);

		let approvals = bitvec![BitOrderLsb0, u8; 1; 4];

		assert_eq!(
			tranches_to_approve(
				&approval_entry,
				&approvals,
				8,
				block_tick,
				no_show_duration,
				needed_approvals,
			),
			RequiredTranches::Pending(8), // tranche_now
		);
	}

	#[test]
	fn tranches_to_approve_cover_no_show_not_enough() {
		let block_tick = 20;
		let no_show_duration = 10;
		let needed_approvals = 4;
		let n_validators = 8;

		let mut approval_entry: ApprovalEntry = approval_db::v1::ApprovalEntry {
			tranches: Vec::new(),
			assignments: bitvec![BitOrderLsb0, u8; 0; n_validators],
			our_assignment: None,
			backing_group: GroupIndex(0),
			approved: false,
		}.into();

		approval_entry.import_assignment(0, 0, block_tick);
		approval_entry.import_assignment(0, 1, block_tick);

		approval_entry.import_assignment(1, 2, block_tick);
		approval_entry.import_assignment(1, 3, block_tick);

		let mut approvals = bitvec![BitOrderLsb0, u8; 0; n_validators];
		approvals.set(0, true);
		approvals.set(1, true);
		// skip 2
		approvals.set(3, true);

		let tranche_now = no_show_duration as DelayTranche + 1;
		assert_eq!(
			tranches_to_approve(
				&approval_entry,
				&approvals,
				tranche_now,
				block_tick,
				no_show_duration,
				needed_approvals,
			),
			RequiredTranches::Pending(2), // tranche 1 + 1 no-show.
		);

		approvals.set(0, false);

		assert_eq!(
			tranches_to_approve(
				&approval_entry,
				&approvals,
				tranche_now,
				block_tick,
				no_show_duration,
				needed_approvals,
			),
			RequiredTranches::Pending(3), // tranche 1 + 2 no-show.
		);
	}

	#[test]
	fn tranches_to_approve_multi_cover_not_enough() {
		let block_tick = 20;
		let no_show_duration = 10;
		let needed_approvals = 4;
		let n_validators = 8;

		let mut approval_entry: ApprovalEntry = approval_db::v1::ApprovalEntry {
			tranches: Vec::new(),
			assignments: bitvec![BitOrderLsb0, u8; 0; n_validators],
			our_assignment: None,
			backing_group: GroupIndex(0),
			approved: false,
		}.into();


		approval_entry.import_assignment(0, 0, block_tick);
		approval_entry.import_assignment(0, 1, block_tick);

		approval_entry.import_assignment(1, 2, block_tick);
		approval_entry.import_assignment(1, 3, block_tick);

		approval_entry.import_assignment(2, 4, block_tick);
		approval_entry.import_assignment(2, 5, block_tick);

		let mut approvals = bitvec![BitOrderLsb0, u8; 0; n_validators];
		approvals.set(0, true);
		approvals.set(1, true);
		// skip 2
		approvals.set(3, true);
		// skip 4
		approvals.set(5, true);

		let tranche_now = no_show_duration as DelayTranche + 1;
		assert_eq!(
			tranches_to_approve(
				&approval_entry,
				&approvals,
				tranche_now,
				block_tick,
				no_show_duration,
				needed_approvals,
			),
			RequiredTranches::Pending(3), // tranche 2 + 1 uncovered no-show
		);
	}

	#[test]
	fn tranches_to_approve_cover_no_show() {
		let block_tick = 20;
		let no_show_duration = 10;
		let needed_approvals = 4;
		let n_validators = 8;

		let mut approval_entry: ApprovalEntry = approval_db::v1::ApprovalEntry {
			tranches: Vec::new(),
			assignments: bitvec![BitOrderLsb0, u8; 0; n_validators],
			our_assignment: None,
			backing_group: GroupIndex(0),
			approved: false,
		}.into();

		approval_entry.import_assignment(0, 0, block_tick);
		approval_entry.import_assignment(0, 1, block_tick);

		approval_entry.import_assignment(1, 2, block_tick);
		approval_entry.import_assignment(1, 3, block_tick);

		approval_entry.import_assignment(2, 4, block_tick);
		approval_entry.import_assignment(2, 5, block_tick);

		let mut approvals = bitvec![BitOrderLsb0, u8; 0; n_validators];
		approvals.set(0, true);
		approvals.set(1, true);
		// skip 2
		approvals.set(3, true);
		approvals.set(4, true);
		approvals.set(5, true);

		let tranche_now = no_show_duration as DelayTranche + 1;
		assert_eq!(
			tranches_to_approve(
				&approval_entry,
				&approvals,
				tranche_now,
				block_tick,
				no_show_duration,
				needed_approvals,
			),
			RequiredTranches::Exact(2, 1),
		);

		// Even though tranche 2 has 2 validators, it only covers 1 no-show.
		// to cover a second no-show, we need to take another non-empty tranche.

		approvals.set(0, false);

		assert_eq!(
			tranches_to_approve(
				&approval_entry,
				&approvals,
				tranche_now,
				block_tick,
				no_show_duration,
				needed_approvals,
			),
			RequiredTranches::Pending(3),
		);

		approval_entry.import_assignment(3, 6, block_tick);
		approvals.set(6, true);

		assert_eq!(
			tranches_to_approve(
				&approval_entry,
				&approvals,
				tranche_now,
				block_tick,
				no_show_duration,
				needed_approvals,
			),
			RequiredTranches::Exact(3, 2),
		);
	}
}