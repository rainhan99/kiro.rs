use super::*;
use crate::gateway::{Amount, BudgetEnforcement, TokenPrices, UpstreamKind};

fn amount(value: &str) -> Amount { value.parse().unwrap() }
fn policy(unit: BillingUnit) -> BudgetPolicy {
    BudgetPolicy { unit, limit: Some(amount("1")), enforcement: BudgetEnforcement::Hard,
        max_in_flight: 8, max_pending: 8, allowed_models: vec![], allowed_upstreams: vec![] }
}
fn prices(unit: BillingUnit) -> TokenPrices {
    TokenPrices { currency: unit, input: amount("1"), output: amount("2"), cache_read: Amount::ZERO,
        cache_write: Amount::ZERO, cache_write_1h: None }
}
fn reservation(id: &str, unit: BillingUnit, bound: &str) -> ReservationInput {
    ReservationInput { request_id: format!("request-{id}"), attempt_id: id.into(), key_id: u64::MAX,
        unit, public_model: "model".into(), upstream_id: "upstream".into(), upper_bound: Some(amount(bound)),
        bound_kind: BoundKind::Guaranteed,
        snapshot: PriceSnapshot { config_revision: 1, price_revision: 2, binding_id: "binding".into(),
            upstream_kind: UpstreamKind::Anthropic, upstream_model: "native".into(),
            cost_prices: Some(prices(BillingUnit::Usd)), sell_prices: Some(prices(unit)) } }
}
fn settlement(id: &str, charge: &str) -> SettlementInput {
    SettlementInput { settlement_id: format!("settlement-{id}"), attempt_id: id.into(),
        evidence: EvidenceStatus::Confirmed, downstream_amount: Some(amount(charge)),
        attempt_cost: AttemptCost { unit: BillingUnit::Usd, evidence: EvidenceStatus::Confirmed, amount: Some(amount("0.2")) },
        committed: true, outcome: AttemptOutcome::Succeeded, usage: None }
}

// Catches quota cross-talk, missing reservation arithmetic and double debit.
#[test]
fn independent_units_reserve_atomically_and_settle_once() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger.set_account(u64::MAX, policy(BillingUnit::Cny)).unwrap();
    ledger.set_account(u64::MAX, policy(BillingUnit::Usd)).unwrap();
    ledger.reserve(reservation("first", BillingUnit::Cny, "0.7")).unwrap();
    assert!(ledger.reserve(reservation("second", BillingUnit::Cny, "0.4")).is_err());
    ledger.reserve(reservation("dollars", BillingUnit::Usd, "0.4")).unwrap();
    let settled = settlement("first", "0.5");
    ledger.settle(settled.clone()).unwrap();
    ledger.settle(settled.clone()).unwrap();
    let mut conflict = settled;
    conflict.downstream_amount = Some(amount("0.6"));
    assert!(ledger.settle(conflict).is_err());
    let accounts = ledger.accounts(u64::MAX).unwrap();
    let cny = accounts.iter().find(|a| a.policy.unit == BillingUnit::Cny).unwrap();
    assert_eq!(cny.used, amount("0.5"));
    assert_eq!(cny.reserved, Amount::ZERO);
    assert_eq!(cny.available, Some(amount("0.5")));
    let usd = accounts.iter().find(|a| a.policy.unit == BillingUnit::Usd).unwrap();
    assert_eq!(usd.used, Amount::ZERO);
    assert_eq!(usd.reserved, amount("0.4"));
}

fn pending_record(id: &str, committed: bool) -> AttemptInput {
    AttemptInput { event_id: format!("event-{id}"), attempt_id: id.into(),
        cost: AttemptCost { unit: BillingUnit::Usd, evidence: EvidenceStatus::Pending, amount: None },
        committed, outcome: AttemptOutcome::Interrupted, usage: Some(serde_json::json!({"input_tokens": 17})) }
}
fn db_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("kiro-ledger-{}.sqlite", uuid::Uuid::new_v4()))
}

// Catches fail-open admission and configuration that erases used or relabels units.
#[test]
fn admission_validates_policy_bounds_zero_quota_and_authorization() {
    let db = Ledger::open_in_memory().unwrap();
    assert!(db.reserve(reservation("missing", BillingUnit::Cny, "0")).is_err());
    assert!(db.set_account(1, policy(BillingUnit::KiroCredit)).is_err());
    let mut p = policy(BillingUnit::Cny);
    p.max_in_flight = 0;
    assert!(db.set_account(u64::MAX, p.clone()).is_err());
    p.max_in_flight = 1;
    p.max_pending = 0;
    assert!(db.set_account(u64::MAX, p.clone()).is_err());
    p.max_pending = 1;
    p.limit = Some(Amount::ZERO);
    db.set_account(u64::MAX, p.clone()).unwrap();
    assert!(db.reserve(reservation("zero", BillingUnit::Cny, "0")).is_err());
    p.limit = None;
    db.set_account(u64::MAX, p.clone()).unwrap();
    let mut unknown = reservation("unknown", BillingUnit::Cny, "0");
    unknown.upper_bound = None;
    unknown.bound_kind = BoundKind::Unknown;
    assert!(db.reserve(unknown.clone()).is_err());
    p.allowed_models = vec!["other".into()];
    db.set_account(u64::MAX, p.clone()).unwrap();
    assert!(db.reserve(reservation("forbidden", BillingUnit::Cny, "0")).is_err());
    p.allowed_models.clear();
    p.allowed_upstreams = vec!["other".into()];
    db.set_account(u64::MAX, p.clone()).unwrap();
    assert!(db.reserve(reservation("forbidden", BillingUnit::Cny, "0")).is_err());
    p.allowed_upstreams.clear();
    p.enforcement = BudgetEnforcement::Soft;
    db.set_account(u64::MAX, p.clone()).unwrap();
    db.reserve(unknown).unwrap();
    assert!(db.reserve(reservation("concurrency", BillingUnit::Cny, "0")).is_err());
    p.enforcement = BudgetEnforcement::Hard;
    assert!(db.set_account(u64::MAX, p).is_err());
}

// Catches losing reservations/evidence across restart or treating a crash as zero.
#[test]
fn file_reopen_recovery_keeps_customer_exposure_and_exact_settlement() {
    let path = db_path();
    {
        let db = Ledger::open(&path).unwrap();
        db.set_account(u64::MAX, policy(BillingUnit::Cny)).unwrap();
        db.reserve(reservation("crash", BillingUnit::Cny, "0.7")).unwrap();
    }
    {
        let db = Ledger::open(&path).unwrap();
        assert_eq!(db.recover_inflight().unwrap(), 1);
        assert_eq!(db.recover_inflight().unwrap(), 0);
        let a = db.accounts(u64::MAX).unwrap().remove(0);
        assert_eq!(a.used, Amount::ZERO);
        assert_eq!(a.reserved, amount("0.7"));
        assert_eq!((a.in_flight, a.customer_pending, a.provider_only_pending), (0,1,0));
        let mut p = policy(BillingUnit::Cny);
        p.max_pending = 1;
        db.set_account(u64::MAX, p).unwrap();
        assert!(db.reserve(reservation("blocked", BillingUnit::Cny, "0.1")).is_err());
        assert!(db.release("crash").is_err());
        let mut s = settlement("crash", "0.123456789012345678");
        s.outcome = AttemptOutcome::Interrupted;
        db.settle(s).unwrap();
    }
    let db = Ledger::open(&path).unwrap();
    let mut s = settlement("crash", "0.123456789012345678");
    s.outcome = AttemptOutcome::Interrupted;
    db.settle(s).unwrap();
    assert_eq!(db.accounts(u64::MAX).unwrap()[0].used, amount("0.123456789012345678"));
    assert_eq!(db.list_requests(u64::MAX, 10).unwrap()[0].attempts[0].state, ReservationState::Settled);
    let connection = rusqlite::Connection::open(&path).unwrap();
    let stored: (String,String) = connection.query_row("SELECT typeof(used),used FROM ledger_accounts", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(stored, ("text".into(), "0.123456789012345678".into()));
}

// Catches provider unknown costs being debited to customers or erased by fallback.
#[test]
fn hidden_failure_releases_for_independent_currency_retry_and_preserves_cost() {
    let db = Ledger::open_in_memory().unwrap();
    db.set_account(u64::MAX, policy(BillingUnit::Cny)).unwrap();
    db.set_account(u64::MAX, policy(BillingUnit::Usd)).unwrap();
    db.reserve(reservation("hidden", BillingUnit::Cny, "0.7")).unwrap();
    let mut next = reservation("retry", BillingUnit::Usd, "0.4");
    next.request_id = "request-hidden".into();
    assert!(db.reserve(next.clone()).is_err());
    let record = pending_record("hidden", false);
    db.record_attempt(record.clone()).unwrap();
    db.record_attempt(record).unwrap();
    db.release("hidden").unwrap();
    db.release("hidden").unwrap();
    db.reserve(next).unwrap();
    db.settle(settlement("retry", "0.3")).unwrap();
    let a = db.accounts(u64::MAX).unwrap();
    let cny = a.iter().find(|a| a.policy.unit == BillingUnit::Cny).unwrap();
    assert_eq!((cny.used,cny.reserved,cny.customer_pending,cny.provider_only_pending), (Amount::ZERO,Amount::ZERO,0,1));
    let mut resolved = pending_record("hidden", false);
    resolved.event_id = "resolved-provider".into();
    resolved.cost.evidence = EvidenceStatus::Confirmed;
    resolved.cost.amount = Some(amount("0.9"));
    db.record_attempt(resolved).unwrap();
    let requests = db.list_requests(u64::MAX, 10).unwrap();
    assert_eq!(requests.len(),1);
    assert_eq!(requests[0].attempts.len(),2);
    assert_eq!(requests[0].attempts[0].record.as_ref().unwrap().cost.amount,Some(amount("0.9")));
    assert_eq!(requests[0].committed_attempt_id.as_deref(),Some("retry"));
    assert!(requests[0].events.iter().any(|e| e.event_id == "event-hidden"));
    assert!(db.settle(settlement("hidden","0.1")).is_err());
}

// Catches pending->confirmed erasing exposure, release-after-commit, or repeat debit.
#[test]
fn committed_partial_usage_stays_pending_then_resolves_once() {
    let db = Ledger::open_in_memory().unwrap();
    db.set_account(u64::MAX, policy(BillingUnit::Cny)).unwrap();
    db.reserve(reservation("partial", BillingUnit::Cny,"0.7")).unwrap();
    let mut s = settlement("partial","0");
    s.evidence = EvidenceStatus::Pending;
    s.downstream_amount = None;
    s.outcome = AttemptOutcome::Interrupted;
    s.attempt_cost = pending_record("partial",true).cost;
    db.settle(s.clone()).unwrap();
    db.settle(s.clone()).unwrap();
    assert!(db.release("partial").is_err());
    assert_eq!(db.accounts(u64::MAX).unwrap()[0].reserved,amount("0.7"));
    s.settlement_id = "resolved-partial".into();
    s.evidence = EvidenceStatus::Confirmed;
    s.downstream_amount = Some(amount("0.4"));
    db.settle(s.clone()).unwrap();
    db.settle(s).unwrap();
    let a = db.accounts(u64::MAX).unwrap().remove(0);
    assert_eq!((a.used,a.reserved,a.customer_pending,a.provider_only_pending),(amount("0.4"),Amount::ZERO,0,1));
}

// Catches migration overwrites after restart and reuse of historical key IDs.
#[test]
fn legacy_import_is_idempotent_and_preserves_existing_accounts() {
    let path = db_path();
    {
        let db = Ledger::open(&path).unwrap();
        assert_eq!(db.max_recorded_key_id().unwrap(),None);
        let a = db.import_legacy(9,amount("0.0169543708291874"),Some(amount("2"))).unwrap();
        assert_eq!(a.used,amount("0.0169543708291874"));
        db.import_legacy(0,Amount::ZERO,None).unwrap();
        assert_eq!(db.max_recorded_key_id().unwrap(),Some(9));
    }
    let db = Ledger::open(&path).unwrap();
    let a = db.import_legacy(9,amount("0.0169543708291874"),Some(amount("2"))).unwrap();
    assert_eq!(a.used,amount("0.0169543708291874"));
    assert!(db.import_legacy(9,amount("1"),None).is_err());
    let mut p = policy(BillingUnit::KiroCredit);
    p.enforcement = BudgetEnforcement::Soft;
    p.limit = Some(amount("3"));
    db.set_account(u64::MAX,p).unwrap();
    let a = db.import_legacy(u64::MAX,amount("7"),Some(amount("8"))).unwrap();
    assert_eq!((a.used,a.policy.limit),(Amount::ZERO,Some(amount("3"))));
    assert_eq!(db.max_recorded_key_id().unwrap(),Some(u64::MAX));
}

// Catches silent counter reset, unaudited underflow, or rolling pending into a new cycle.
#[test]
fn audited_adjustments_and_cycles_cannot_erase_unresolved_obligations() {
    let db = Ledger::open_in_memory().unwrap();
    db.set_account(u64::MAX,policy(BillingUnit::Cny)).unwrap();
    db.reserve(reservation("adjust",BillingUnit::Cny,"0.7")).unwrap();
    let cycle = NewCycleInput { operation_id:"cycle1".into(), key_id:u64::MAX, unit:BillingUnit::Cny, reason:"new month".into() };
    assert!(db.new_cycle(cycle.clone()).is_err());
    db.settle(settlement("adjust","0.5")).unwrap();
    let adjustment = AdjustmentInput { adjustment_id:"refund".into(),key_id:u64::MAX,unit:BillingUnit::Cny,
        direction:AdjustmentDirection::Credit,amount:amount("0.2"),reason:"invoice correction".into() };
    db.adjust(adjustment.clone()).unwrap();
    db.adjust(adjustment.clone()).unwrap();
    assert_eq!(db.accounts(u64::MAX).unwrap()[0].used,amount("0.3"));
    let mut invalid=adjustment.clone(); invalid.adjustment_id="bad".into(); invalid.amount=amount("0.4");
    assert!(db.adjust(invalid).is_err());
    let mut invalid=adjustment; invalid.adjustment_id="blank".into(); invalid.reason=" ".into();
    assert!(db.adjust(invalid).is_err());
    let a=db.new_cycle(cycle.clone()).unwrap();
    assert_eq!((a.cycle,a.used),(2,Amount::ZERO));
    assert_eq!(db.new_cycle(cycle).unwrap().cycle,2);
    assert_eq!(db.list_requests(u64::MAX,10).unwrap()[0].attempts[0].cycle,1);
    let audit=db.list_account_audit(u64::MAX,10).unwrap();
    assert_eq!(audit.len(),2);
    assert_eq!(audit[0].before.as_ref().unwrap().used,amount("0.3"));
    assert_eq!(audit[0].after.used,Amount::ZERO);
    assert_eq!(audit[1].reason,"invoice correction");
}

// Catches connection-local locking without database-level atomicity.
#[test]
fn concurrent_connections_cannot_oversubscribe() {
    let path=db_path();
    Ledger::open(&path).unwrap().set_account(u64::MAX,policy(BillingUnit::Cny)).unwrap();
    let db1=Ledger::open(&path).unwrap(); let db2=Ledger::open(&path).unwrap();
    let barrier=std::sync::Arc::new(std::sync::Barrier::new(2));
    let b1=barrier.clone();
    let one=std::thread::spawn(move || { b1.wait(); db1.reserve(reservation("one",BillingUnit::Cny,"0.7")).is_ok() });
    let two=std::thread::spawn(move || { barrier.wait(); db2.reserve(reservation("two",BillingUnit::Cny,"0.7")).is_ok() });
    assert_ne!(one.join().unwrap(),two.join().unwrap());
    assert_eq!(Ledger::open(&path).unwrap().accounts(u64::MAX).unwrap()[0].reserved,amount("0.7"));
}

// Catches fallback-to-memory after opening an invalid financial database.
#[test]
fn financial_open_errors_fail_closed() {
    assert!(Ledger::open(":memory:").is_err());
    assert!(Ledger::open("file:ledger?mode=memory&cache=shared").is_err());
    assert!(Ledger::open("").is_err());
    assert!(Ledger::open(std::env::temp_dir()).is_err());
    assert!(Ledger::open(db_path().join("no-parent.sqlite")).is_err());
}

// Catches unlimited-account overflow poisoning otherwise valid persisted state.
#[test]
fn reservation_overflow_rolls_back_atomically() {
    let db=Ledger::open_in_memory().unwrap();
    let mut p=policy(BillingUnit::Cny); p.limit=None;
    db.set_account(u64::MAX,p).unwrap();
    db.reserve(reservation("maximum",BillingUnit::Cny,"170141183460469231731.687303715884105727")).unwrap();
    assert!(db.reserve(reservation("overflow",BillingUnit::Cny,"0.000000000000000001")).is_err());
    assert_eq!(db.list_requests(u64::MAX,10).unwrap().len(),1);
    assert_eq!(db.accounts(u64::MAX).unwrap()[0].reserved,amount("170141183460469231731.687303715884105727"));
}

// Catches hard-bound bypass and non-transactional commitment on rejected evidence.
#[test]
fn rejected_settlement_does_not_change_usage_or_commit_request() {
    let db=Ledger::open_in_memory().unwrap();
    db.set_account(u64::MAX,policy(BillingUnit::Cny)).unwrap();
    db.reserve(reservation("reject",BillingUnit::Cny,"0.7")).unwrap();
    assert!(db.settle(settlement("reject","0.8")).is_err());
    let mut wrong=settlement("reject","0.4"); wrong.attempt_cost.unit=BillingUnit::Cny;
    assert!(db.settle(wrong).is_err());
    let mut hidden=settlement("reject","0.4"); hidden.committed=false; hidden.outcome=AttemptOutcome::Failed;
    assert!(db.settle(hidden).is_err());
    let mut missing=settlement("reject","0.4"); missing.downstream_amount=None;
    assert!(db.settle(missing).is_err());
    let request=&db.list_requests(u64::MAX,10).unwrap()[0];
    assert_eq!(request.committed_attempt_id,None);
    assert_eq!(request.events.len(),0);
    assert_eq!(request.attempts[0].record,None);
    assert_eq!(db.accounts(u64::MAX).unwrap()[0].used,Amount::ZERO);
    db.settle(settlement("reject","0.5")).unwrap();
    let mut changed=settlement("reject","0.6"); changed.settlement_id="different-id".into();
    assert!(db.settle(changed).is_err());
}

// Catches accidental native-credit bound claims and rounding of soft overage.
#[test]
fn native_credit_soft_overage_is_exact_and_blocks_further_admission() {
    let db=Ledger::open_in_memory().unwrap();
    let mut p=policy(BillingUnit::KiroCredit); p.enforcement=BudgetEnforcement::Soft; p.limit=Some(amount("0.01"));
    db.set_account(u64::MAX,p).unwrap();
    let mut r=reservation("credit",BillingUnit::KiroCredit,"0.01");
    r.snapshot.upstream_kind=UpstreamKind::Kiro; r.snapshot.cost_prices=None; r.snapshot.sell_prices=None;
    assert!(db.reserve(r.clone()).is_err());
    r.bound_kind=BoundKind::Unknown; r.upper_bound=None;
    db.reserve(r.clone()).unwrap();
    let mut s=settlement("credit","0.0169543708291874"); s.attempt_cost.unit=BillingUnit::KiroCredit;
    s.attempt_cost.amount=Some(amount("0.0169543708291874"));
    db.settle(s).unwrap();
    let a=&db.accounts(u64::MAX).unwrap()[0];
    assert_eq!(a.used,amount("0.0169543708291874")); assert_eq!(a.available,Some(Amount::ZERO));
    r.request_id="next-credit".into(); r.attempt_id="next-credit".into();
    assert!(db.reserve(r).is_err());
}

// Catches a restarted blanket migration granting money-only keys credit access.
#[test]
fn migration_seal_is_durable_and_blocks_fresh_legacy_imports() {
    let path=db_path();
    {
        let db=Ledger::open(&path).unwrap();
        assert!(!db.legacy_migration_complete().unwrap());
        db.import_legacy(0,amount("0.5"),None).unwrap();
        db.mark_legacy_migration_complete().unwrap();
        db.mark_legacy_migration_complete().unwrap();
    }
    let db=Ledger::open(&path).unwrap();
    assert!(db.legacy_migration_complete().unwrap());
    db.set_account(2,policy(BillingUnit::Cny)).unwrap();
    assert!(db.import_legacy(2,Amount::ZERO,None).is_err());
    db.import_legacy(0,amount("0.5"),None).unwrap();
    assert_eq!(db.accounts(2).unwrap().len(),1);
}

// Catches cycle change deleting pending costs or letting policy updates reset usage.
#[test]
fn provider_only_pending_survives_cycle_and_account_update_preserves_usage() {
    let db=Ledger::open_in_memory().unwrap();
    db.set_account(u64::MAX,policy(BillingUnit::Cny)).unwrap();
    db.reserve(reservation("pending-cost",BillingUnit::Cny,"0.7")).unwrap();
    let mut s=settlement("pending-cost","0.5"); s.attempt_cost=pending_record("pending-cost",true).cost;
    db.settle(s).unwrap();
    let mut p=policy(BillingUnit::Cny); p.limit=Some(amount("2"));
    assert_eq!(db.set_account(u64::MAX,p.clone()).unwrap().used,amount("0.5"));
    let a=db.new_cycle(NewCycleInput {operation_id:"new-cycle".into(),key_id:u64::MAX,unit:BillingUnit::Cny,reason:"monthly close".into()}).unwrap();
    assert_eq!((a.cycle,a.used,a.provider_only_pending),(2,Amount::ZERO,1));
    p.max_pending=1;
    db.set_account(u64::MAX,p).unwrap();
    assert!(db.reserve(reservation("pending-gate",BillingUnit::Cny,"0.1")).is_err());
    let mut evidence=pending_record("pending-cost",true); evidence.outcome=AttemptOutcome::Succeeded;
    evidence.cost.evidence=EvidenceStatus::Confirmed; evidence.cost.amount=Some(amount("0.8"));
    db.record_attempt(evidence).unwrap();
    assert_eq!(db.accounts(u64::MAX).unwrap()[0].provider_only_pending,0);
    assert_eq!(db.accounts(u64::MAX).unwrap()[0].used,Amount::ZERO);
    db.reserve(reservation("pending-gate",BillingUnit::Cny,"0.1")).unwrap();
}
