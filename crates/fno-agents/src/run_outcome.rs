use crate::loopcheck::TerminationReason;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub outcome: Option<Outcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Committed(Commit),
    Aborted(Abort),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub deliverable: Deliverable,
    pub shipped: bool,
    pub node_closable: bool,
    pub merge_armable: bool,
    pub outstanding: Outstanding,
    pub progress: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Abort {
    pub cause: AbortCause,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbortCause {
    Budget,
    NoProgress,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deliverable {
    None,
    Pr,
    Doc,
    BatchMember,
    Delivery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outstanding {
    Nothing,
    HumanMerge { cause: HumanMergeCause },
    Review { cause: ReviewCause },
    BatchPr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanMergeCause {
    PreExistingMainRed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewCause {
    NobodyReviewed,
    BotRateLimited,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredicateProjection {
    pub ship_reason: bool,
    pub do_stamp_terminal: bool,
    pub merge_armable: bool,
    pub delivery_ship: bool,
    pub graduate: bool,
    pub stuck: bool,
    pub record_ledger: bool,
    pub circuit_breaker_success: bool,
    pub awaiting_review_notify: bool,
}

impl RunOutcome {
    pub fn projection(&self) -> PredicateProjection {
        let commit = match &self.outcome {
            Some(Outcome::Committed(commit)) => Some(commit),
            _ => None,
        };
        let outstanding = commit.map(|value| &value.outstanding);

        PredicateProjection {
            ship_reason: commit.is_some_and(|value| value.shipped),
            do_stamp_terminal: commit
                .is_some_and(|value| matches!(value.deliverable, Deliverable::Pr)),
            merge_armable: commit.is_some_and(|value| value.merge_armable),
            delivery_ship: commit
                .is_some_and(|value| matches!(value.deliverable, Deliverable::Delivery)),
            graduate: commit.is_some_and(|value| {
                value.shipped && matches!(value.deliverable, Deliverable::Doc)
            }),
            stuck: matches!(
                self.outcome,
                Some(Outcome::Aborted(_)) | Some(Outcome::Cancelled)
            ),
            record_ledger: self.outcome.is_some(),
            circuit_breaker_success: matches!(
                outstanding,
                Some(Outstanding::HumanMerge { .. })
                    | Some(Outstanding::Review {
                        cause: ReviewCause::BotRateLimited
                    })
            ),
            awaiting_review_notify: matches!(
                outstanding,
                Some(Outstanding::Review {
                    cause: ReviewCause::NobodyReviewed
                })
            ),
        }
    }
}

pub fn classify(reason: TerminationReason) -> RunOutcome {
    let outcome = match reason {
        TerminationReason::DonePRGreen => Outcome::Committed(commit(
            Deliverable::Pr,
            true,
            true,
            true,
            Outstanding::Nothing,
        )),
        TerminationReason::DoneAdvisory => Outcome::Committed(commit(
            Deliverable::Doc,
            true,
            true,
            false,
            Outstanding::Nothing,
        )),
        TerminationReason::DoneDelivery => Outcome::Committed(commit(
            Deliverable::Delivery,
            false,
            true,
            false,
            Outstanding::Nothing,
        )),
        TerminationReason::DoneBatched => Outcome::Committed(commit(
            Deliverable::BatchMember,
            false,
            false,
            false,
            Outstanding::BatchPr,
        )),
        TerminationReason::DoneAwaitingMerge => Outcome::Committed(commit(
            Deliverable::Pr,
            false,
            false,
            false,
            Outstanding::HumanMerge {
                cause: HumanMergeCause::PreExistingMainRed,
            },
        )),
        TerminationReason::DoneUnreviewed => Outcome::Committed(commit(
            Deliverable::Pr,
            false,
            false,
            false,
            Outstanding::Review {
                cause: ReviewCause::NobodyReviewed,
            },
        )),
        TerminationReason::DoneAwaitingReview => Outcome::Committed(commit(
            Deliverable::Pr,
            false,
            false,
            false,
            Outstanding::Review {
                cause: ReviewCause::BotRateLimited,
            },
        )),
        TerminationReason::DonePlanned => Outcome::Committed(commit(
            Deliverable::None,
            false,
            false,
            false,
            Outstanding::Nothing,
        )),
        TerminationReason::NoWork => {
            return RunOutcome { outcome: None };
        }
        TerminationReason::Budget => Outcome::Aborted(Abort {
            cause: AbortCause::Budget,
        }),
        TerminationReason::NoProgress => Outcome::Aborted(Abort {
            cause: AbortCause::NoProgress,
        }),
        TerminationReason::Interrupted => Outcome::Cancelled,
        TerminationReason::Aborted => Outcome::Aborted(Abort {
            cause: AbortCause::Failed,
        }),
    };

    RunOutcome {
        outcome: Some(outcome),
    }
}

fn commit(
    deliverable: Deliverable,
    shipped: bool,
    node_closable: bool,
    merge_armable: bool,
    outstanding: Outstanding,
) -> Commit {
    Commit {
        deliverable,
        shipped,
        node_closable,
        merge_armable,
        outstanding,
        progress: true,
    }
}

pub fn legacy_projection(reason: &TerminationReason) -> PredicateProjection {
    PredicateProjection {
        ship_reason: matches!(
            reason,
            TerminationReason::DonePRGreen | TerminationReason::DoneAdvisory
        ),
        do_stamp_terminal: matches!(
            reason,
            TerminationReason::DonePRGreen
                | TerminationReason::DoneAwaitingMerge
                | TerminationReason::DoneUnreviewed
                | TerminationReason::DoneAwaitingReview
        ),
        merge_armable: matches!(reason, TerminationReason::DonePRGreen),
        delivery_ship: matches!(reason, TerminationReason::DoneDelivery),
        graduate: matches!(reason, TerminationReason::DoneAdvisory),
        stuck: matches!(
            reason,
            TerminationReason::NoProgress
                | TerminationReason::Budget
                | TerminationReason::Interrupted
                | TerminationReason::Aborted
        ),
        record_ledger: !matches!(reason, TerminationReason::NoWork),
        circuit_breaker_success: matches!(
            reason,
            TerminationReason::DoneAwaitingMerge | TerminationReason::DoneAwaitingReview
        ),
        awaiting_review_notify: matches!(reason, TerminationReason::DoneUnreviewed),
    }
}
