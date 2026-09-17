//! Financial state is independent of traces. Every operation uses a SQLite
//! IMMEDIATE transaction, including admission arithmetic performed in Rust.
use std::{path::Path, time::Duration};
use anyhow::{Context, ensure};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};
pub use super::ledger_types::*;
use super::{Amount, BillingUnit, BudgetEnforcement, BudgetPolicy, UpstreamKind};

pub struct Ledger { connection: Mutex<Connection> }

impl Ledger {
    pub fn legacy_migration_complete(&self) -> anyhow::Result<bool> { self.transaction(migration_complete) }
    pub fn mark_legacy_migration_complete(&self) -> anyhow::Result<()> {
        self.transaction(|tx| {
            tx.execute("INSERT INTO ledger_metadata(name,value) VALUES('legacy_migration_complete','true') ON CONFLICT(name) DO NOTHING", [])?;
            Ok(())
        })
    }
    pub fn list_account_audit(&self, key_id: u64, limit: u32) -> anyhow::Result<Vec<AccountAuditView>> {
        self.transaction(|tx| {
            let rows = tx.prepare("SELECT data FROM ledger_account_audit WHERE key_id=?1 ORDER BY rowid DESC LIMIT ?2")?
                .query_map(params![key_id.to_string(), limit], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            rows.iter().map(|s| decode(s)).collect()
        })
    }
    pub fn record_attempt(&self, input: AttemptInput) -> anyhow::Result<AttemptView> {
        nonblank(&input.event_id)?;
        self.transaction(|tx| {
            let mut view = attempt(tx, &input.attempt_id)?;
            if event_matches(tx, &input.event_id, "attempt", &input)? { return Ok(view); }
            update_record(tx, &mut view, input.clone())?;
            if holds_customer(&view) { view.state = ReservationState::Pending; }
            save_attempt(tx, &view)?;
            insert_event(tx, &input.event_id, "attempt", &input)?;
            Ok(view)
        })
    }
    /// Requires an explicit record proving no downstream commitment. Unknown
    /// crash records cannot be released merely because no callback arrived.
    pub fn release(&self, attempt_id: &str) -> anyhow::Result<AttemptView> {
        self.transaction(|tx| {
            let mut view = attempt(tx, attempt_id)?;
            let record = view.record.as_ref().context("release requires a recorded uncommitted failure")?;
            ensure!(!record.committed && record.outcome != AttemptOutcome::Succeeded, "committed attempt cannot release");
            ensure!(view.state != ReservationState::Settled, "settled attempt cannot release");
            if view.state != ReservationState::Released {
                view.state = ReservationState::Released;
                save_attempt(tx, &view)?;
                insert_event(tx, &format!("release:{}", uuid::Uuid::new_v4()), "release",
                    &serde_json::json!({"attemptId":attempt_id,"reason":"uncommitted failed attempt"}))?;
            }
            Ok(view)
        })
    }
    /// Explicit startup step, before accepting new work; never settles to zero.
    pub fn recover_inflight(&self) -> anyhow::Result<u64> {
        self.transaction(|tx| {
            let rows = tx.prepare("SELECT data FROM ledger_attempts ORDER BY rowid")?
                .query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            let mut count=0_u64;
            for row in rows {
                let mut view: AttemptView=decode(&row)?;
                if view.state == ReservationState::InFlight {
                    view.state=ReservationState::Pending;
                    save_attempt(tx,&view)?;
                    insert_event(tx,&format!("recovery:{}",uuid::Uuid::new_v4()),"recovery",
                        &serde_json::json!({"attemptId":view.reservation.attempt_id,"reason":"startup unresolved attempt"}))?;
                    count=count.checked_add(1).context("recovery count overflow")?;
                }
            }
            Ok(count)
        })
    }
    pub fn list_requests(&self, key_id: u64, limit: u32) -> anyhow::Result<Vec<RequestView>> {
        self.transaction(|tx| {
            let ids=tx.prepare("SELECT request_id FROM ledger_requests WHERE key_id=?1 ORDER BY rowid DESC LIMIT ?2")?
                .query_map(params![key_id.to_string(),limit],|r| r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?;
            ids.iter().map(|id| request_optional(tx,id)?.context("request disappeared")).collect()
        })
    }
    /// Imports only an absent native-credit account. Existing accounts are
    /// retained verbatim and the skipped opening balance is still audited.
    pub fn import_legacy(&self, key_id: u64, used: Amount, limit: Option<Amount>) -> anyhow::Result<AccountView> {
        self.transaction(|tx| {
            let id=format!("legacy-opening:{key_id}");
            let input=serde_json::json!({"keyId":key_id,"used":used,"limit":limit,"source":"legacy client-key opening balance"});
            if event_matches(tx,&id,"legacy_import",&input)? { return account(tx,key_id,BillingUnit::KiroCredit); }
            ensure!(!migration_complete(tx)?, "legacy migration is sealed");
            let exists: bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ledger_accounts WHERE key_id=?1 AND unit='kiro_credit')",
                [key_id.to_string()],|r| r.get(0))?;
            let before=if exists { Some(account(tx,key_id,BillingUnit::KiroCredit)?) } else { None };
            if !exists {
                let policy=BudgetPolicy { unit:BillingUnit::KiroCredit, limit, enforcement:BudgetEnforcement::Soft,
                    max_in_flight:1,max_pending:8,allowed_models:vec![],allowed_upstreams:vec![] };
                tx.execute("INSERT INTO ledger_accounts(key_id,unit,policy,cycle,used) VALUES(?1,'kiro_credit',?2,1,?3)",
                    params![key_id.to_string(),json(&policy)?,used.to_string()])?;
            }
            let after=account(tx,key_id,BillingUnit::KiroCredit)?;
            insert_event(tx,&id,"legacy_import",&input)?;
            save_audit(tx,AccountAuditView { operation_id:id,kind:"legacy_import".into(),key_id,unit:BillingUnit::KiroCredit,
                reason:if exists { "legacy opening balance skipped: account already exists" } else { "legacy client-key opening balance; not historical request usage" }.into(),
                before,after:after.clone() })?;
            Ok(after)
        })
    }
    pub fn adjust(&self, input: AdjustmentInput) -> anyhow::Result<AccountView> {
        nonblank(&input.adjustment_id)?; nonblank(&input.reason)?;
        self.transaction(|tx| {
            let before=account(tx,input.key_id,input.unit)?;
            if event_matches(tx,&input.adjustment_id,"adjustment",&input)? { return Ok(before); }
            let used=match input.direction {
                AdjustmentDirection::Debit => before.used.checked_add(input.amount)?,
                AdjustmentDirection::Credit => before.used.checked_sub(input.amount)?,
            };
            set_used(tx,&before,used)?;
            let after=account(tx,input.key_id,input.unit)?;
            insert_event(tx,&input.adjustment_id,"adjustment",&input)?;
            save_audit(tx,AccountAuditView { operation_id:input.adjustment_id.clone(),kind:"adjustment".into(),
                key_id:input.key_id,unit:input.unit,reason:input.reason.clone(),before:Some(before),after:after.clone() })?;
            Ok(after)
        })
    }
    pub fn new_cycle(&self, input: NewCycleInput) -> anyhow::Result<AccountView> {
        nonblank(&input.operation_id)?; nonblank(&input.reason)?;
        self.transaction(|tx| {
            let before=account(tx,input.key_id,input.unit)?;
            if event_matches(tx,&input.operation_id,"new_cycle",&input)? { return Ok(before); }
            ensure!(before.in_flight==0 && before.customer_pending==0,"cannot change cycle with customer obligations");
            let next=before.cycle.checked_add(1).context("cycle overflow")?;
            tx.execute("INSERT INTO ledger_cycles(key_id,unit,cycle,closing_account) VALUES(?1,?2,?3,?4)",
                params![input.key_id.to_string(),unit_name(input.unit),before.cycle,json(&before)?])?;
            tx.execute("UPDATE ledger_accounts SET cycle=?1,used='0' WHERE key_id=?2 AND unit=?3",
                params![next,input.key_id.to_string(),unit_name(input.unit)])?;
            let after=account(tx,input.key_id,input.unit)?;
            insert_event(tx,&input.operation_id,"new_cycle",&input)?;
            save_audit(tx,AccountAuditView { operation_id:input.operation_id.clone(),kind:"new_cycle".into(),
                key_id:input.key_id,unit:input.unit,reason:input.reason.clone(),before:Some(before),after:after.clone() })?;
            Ok(after)
        })
    }
    /// Account owners are permanent: deleting a client key must never reuse its
    /// identity. Compare as u64 in Rust, not SQL TEXT lexicographic MAX.
    pub fn max_recorded_key_id(&self) -> anyhow::Result<Option<u64>> {
        self.transaction(|tx| {
            let rows=tx.prepare("SELECT DISTINCT key_id FROM ledger_accounts")?
                .query_map([],|r| r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?;
            Ok(rows.iter().map(|s| s.parse::<u64>()).collect::<Result<Vec<_>,_>>()?.into_iter().max())
        })
    }
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        ensure!(!path.as_os_str().is_empty() && path != Path::new(":memory:") && !path.to_string_lossy().starts_with("file:"), "persistent ledger requires ordinary file path");
        Self::initialize(Connection::open(path)?)
    }
    pub fn open_in_memory() -> anyhow::Result<Self> { Self::initialize(Connection::open_in_memory()?) }
    fn initialize(mut connection: Connection) -> anyhow::Result<Self> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS ledger_accounts (
              key_id TEXT NOT NULL, unit TEXT NOT NULL, policy TEXT NOT NULL, cycle INTEGER NOT NULL,
              used TEXT NOT NULL CHECK(typeof(used)='text'), PRIMARY KEY(key_id,unit));
             CREATE TABLE IF NOT EXISTS ledger_requests (
              request_id TEXT PRIMARY KEY, key_id TEXT NOT NULL, public_model TEXT NOT NULL,
              committed_attempt_id TEXT, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
             CREATE TABLE IF NOT EXISTS ledger_attempts (
              attempt_id TEXT PRIMARY KEY, request_id TEXT NOT NULL REFERENCES ledger_requests(request_id),
              key_id TEXT NOT NULL, unit TEXT NOT NULL, data TEXT NOT NULL,
              FOREIGN KEY(key_id,unit) REFERENCES ledger_accounts(key_id,unit));
             CREATE INDEX IF NOT EXISTS ledger_attempts_account ON ledger_attempts(key_id,unit);
             CREATE INDEX IF NOT EXISTS ledger_attempts_request ON ledger_attempts(request_id);
             CREATE TABLE IF NOT EXISTS ledger_events (
              event_id TEXT PRIMARY KEY, kind TEXT NOT NULL, payload TEXT NOT NULL,
              created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
             CREATE TABLE IF NOT EXISTS ledger_cycles (
              key_id TEXT NOT NULL, unit TEXT NOT NULL, cycle INTEGER NOT NULL,
              closing_account TEXT NOT NULL, PRIMARY KEY(key_id,unit,cycle));
             CREATE TABLE IF NOT EXISTS ledger_account_audit (
              operation_id TEXT PRIMARY KEY REFERENCES ledger_events(event_id), key_id TEXT NOT NULL, data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS ledger_metadata (name TEXT PRIMARY KEY,value TEXT NOT NULL);")?;
        tx.commit()?;
        Ok(Self { connection: Mutex::new(connection) })
    }
    fn transaction<T>(&self, f: impl FnOnce(&Transaction<'_>) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }
    pub fn set_account(&self, key_id: u64, policy: BudgetPolicy) -> anyhow::Result<AccountView> {
        validate_policy(&policy)?;
        self.transaction(|tx| {
            if policy.enforcement == BudgetEnforcement::Hard {
                for a in account_attempts(tx, key_id, policy.unit)? {
                    ensure!(!holds_customer(&a) || a.reservation.bound_kind == BoundKind::Guaranteed, "unbounded obligations prevent hard enforcement");
                }
            }
            tx.execute("INSERT INTO ledger_accounts(key_id,unit,policy,cycle,used) VALUES(?1,?2,?3,1,'0')
                ON CONFLICT(key_id,unit) DO UPDATE SET policy=excluded.policy",
                params![key_id.to_string(), unit_name(policy.unit), json(&policy)?])?;
            account(tx, key_id, policy.unit)
        })
    }
    pub fn accounts(&self, key_id: u64) -> anyhow::Result<Vec<AccountView>> {
        self.transaction(|tx| {
            let rows = tx.prepare("SELECT policy FROM ledger_accounts WHERE key_id=?1 ORDER BY unit")?
                .query_map([key_id.to_string()], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            rows.iter().map(|p| account(tx, key_id, decode::<BudgetPolicy>(p)?.unit)).collect()
        })
    }
    pub fn reserve(&self, input: ReservationInput) -> anyhow::Result<AttemptView> {
        validate_reservation(&input)?;
        self.transaction(|tx| {
            if let Some(old) = attempt_optional(tx, &input.attempt_id)? {
                ensure!(old.reservation == input, "conflicting reservation");
                return Ok(old);
            }
            let a = account(tx, input.key_id, input.unit)?;
            ensure!(a.policy.limit != Some(Amount::ZERO), "zero quota denies consumption");
            ensure!(a.in_flight < a.policy.max_in_flight, "in-flight limit reached");
            ensure!(a.pending < a.policy.max_pending, "pending limit reached");
            ensure!(allowed(&a.policy.allowed_models, &input.public_model), "model not authorized");
            ensure!(allowed(&a.policy.allowed_upstreams, &input.upstream_id), "upstream not authorized");
            if a.policy.enforcement == BudgetEnforcement::Hard {
                ensure!(input.bound_kind == BoundKind::Guaranteed && input.upper_bound.is_some(), "hard account requires guaranteed bound");
            }
            if let Some(available) = a.available {
                ensure!(available > Amount::ZERO, "quota exhausted");
                ensure!(input.upper_bound.unwrap_or(Amount::ZERO) <= available, "insufficient quota");
            }
            // Validate aggregate representability even for an unlimited account.
            a.used.checked_add(a.reserved)?.checked_add(input.upper_bound.unwrap_or(Amount::ZERO))?;
            if let Some(r) = request_optional(tx, &input.request_id)? {
                ensure!(r.key_id == input.key_id && r.public_model == input.public_model, "request identity conflict");
                ensure!(r.committed_attempt_id.is_none(), "request already committed");
                ensure!(r.attempts.iter().all(|a| !holds_customer(a)), "release prior reservation before retry");
            } else {
                tx.execute("INSERT INTO ledger_requests(request_id,key_id,public_model) VALUES(?1,?2,?3)",
                    params![input.request_id, input.key_id.to_string(), input.public_model])?;
            }
            let view = AttemptView { reservation: input, cycle: a.cycle, enforcement: a.policy.enforcement,
                state: ReservationState::InFlight, record: None, settlement: None };
            tx.execute("INSERT INTO ledger_attempts(attempt_id,request_id,key_id,unit,data) VALUES(?1,?2,?3,?4,?5)",
                params![view.reservation.attempt_id, view.reservation.request_id, view.reservation.key_id.to_string(), unit_name(view.reservation.unit), json(&view)?])?;
            Ok(view)
        })
    }
    pub fn settle(&self, input: SettlementInput) -> anyhow::Result<AttemptView> {
        nonblank(&input.settlement_id)?;
        validate_evidence(input.evidence, input.downstream_amount)?;
        self.transaction(|tx| {
            let mut view = attempt(tx, &input.attempt_id)?;
            if event_matches(tx, &input.settlement_id, "settlement", &input)? { return Ok(view); }
            if let Some(old) = &view.settlement { ensure!(old.evidence != EvidenceStatus::Confirmed, "attempt already finally settled"); }
            ensure!(view.state != ReservationState::Settled, "attempt already settled");
            ensure!(input.committed || input.downstream_amount.is_none_or(|a| a == Amount::ZERO), "hidden attempt cannot debit customer");
            update_record(tx, &mut view, AttemptInput { event_id: input.settlement_id.clone(), attempt_id: input.attempt_id.clone(),
                cost: input.attempt_cost.clone(), committed: input.committed, outcome: input.outcome, usage: input.usage.clone() })?;
            if input.committed && input.evidence == EvidenceStatus::Confirmed {
                let charge = input.downstream_amount.context("confirmed charge missing")?;
                if view.enforcement == BudgetEnforcement::Hard {
                    ensure!(charge <= view.reservation.upper_bound.context("hard bound missing")?, "confirmed charge exceeds guaranteed bound");
                }
                let a = account(tx, view.reservation.key_id, view.reservation.unit)?;
                ensure!(a.cycle == view.cycle, "settlement cycle mismatch");
                set_used(tx, &a, a.used.checked_add(charge)?)?;
                view.state = ReservationState::Settled;
            } else if input.committed { view.state = ReservationState::Pending; }
            else { view.state = ReservationState::Released; }
            view.settlement = Some(input.clone());
            save_attempt(tx, &view)?;
            insert_event(tx, &input.settlement_id, "settlement", &input)?;
            Ok(view)
        })
    }
}

pub fn validate_policy(p: &BudgetPolicy) -> anyhow::Result<()> {
    ensure!(p.max_in_flight > 0 && p.max_pending > 0, "concurrency and pending limits must be positive");
    ensure!(p.unit != BillingUnit::KiroCredit || p.enforcement == BudgetEnforcement::Soft, "native Kiro credits require soft enforcement");
    for values in [&p.allowed_models, &p.allowed_upstreams] {
        let mut unique = std::collections::HashSet::new();
        for v in values { nonblank(v)?; ensure!(unique.insert(v), "duplicate authorization entry"); }
    }
    Ok(())
}
fn validate_reservation(i: &ReservationInput) -> anyhow::Result<()> {
    for v in [&i.request_id, &i.attempt_id, &i.public_model, &i.upstream_id, &i.snapshot.binding_id, &i.snapshot.upstream_model] { nonblank(v)?; }
    ensure!((i.bound_kind == BoundKind::Guaranteed) == i.upper_bound.is_some(), "bound kind and amount disagree");
    if i.unit == BillingUnit::KiroCredit {
        ensure!(i.snapshot.upstream_kind == UpstreamKind::Kiro, "only Kiro provides native credit billing");
        ensure!(i.bound_kind == BoundKind::Unknown, "native credit has no guaranteed bound");
    } else {
        let sale = i.snapshot.sell_prices.as_ref().context("monetary sale prices missing")?;
        sale.validate()?;
        ensure!(sale.currency == i.unit, "sale currency mismatch");
    }
    if i.snapshot.upstream_kind == UpstreamKind::Kiro { ensure!(i.snapshot.cost_prices.is_none(), "Kiro cost uses native credits"); }
    else { i.snapshot.cost_prices.as_ref().context("cost prices missing")?.validate()?; }
    if let Some(p) = &i.snapshot.sell_prices { p.validate()?; }
    Ok(())
}
fn validate_evidence(status: EvidenceStatus, amount: Option<Amount>) -> anyhow::Result<()> {
    ensure!((status == EvidenceStatus::Confirmed) == amount.is_some(), "confirmed evidence requires amount; pending cannot invent amount");
    Ok(())
}
fn update_record(tx: &Transaction<'_>, view: &mut AttemptView, i: AttemptInput) -> anyhow::Result<()> {
    validate_evidence(i.cost.evidence, i.cost.amount)?;
    let unit = if view.reservation.snapshot.upstream_kind == UpstreamKind::Kiro { BillingUnit::KiroCredit }
        else { view.reservation.snapshot.cost_prices.as_ref().context("frozen cost prices missing")?.currency };
    ensure!(i.cost.unit == unit, "provider cost currency mismatch");
    ensure!(i.committed || i.outcome != AttemptOutcome::Succeeded, "success requires committed response");
    ensure!(view.state != ReservationState::Released || !i.committed, "released attempt cannot commit");
    if let Some(old) = &view.record {
        ensure!(!old.committed || i.committed, "commitment cannot reverse");
        if old.cost.evidence == EvidenceStatus::Confirmed { ensure!(old.cost == i.cost, "confirmed provider cost is immutable"); }
        if old.committed { ensure!(old.outcome == i.outcome, "committed outcome is immutable"); }
    }
    if i.committed {
        let committed: Option<String> = tx.query_row("SELECT committed_attempt_id FROM ledger_requests WHERE request_id=?1",
            [&view.reservation.request_id], |r| r.get(0))?;
        ensure!(committed.as_ref().is_none_or(|id| id == &i.attempt_id), "another attempt committed request");
        tx.execute("UPDATE ledger_requests SET committed_attempt_id=?1 WHERE request_id=?2", params![i.attempt_id, view.reservation.request_id])?;
    }
    view.record = Some(i);
    Ok(())
}
fn account(tx: &Transaction<'_>, key: u64, unit: BillingUnit) -> anyhow::Result<AccountView> {
    let (policy, cycle, used): (String, u64, String) = tx.query_row("SELECT policy,cycle,used FROM ledger_accounts WHERE key_id=?1 AND unit=?2",
        params![key.to_string(), unit_name(unit)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?.context("billing account missing")?;
    let policy: BudgetPolicy = decode(&policy)?;
    let used: Amount = used.parse()?;
    let (mut reserved, mut in_flight, mut customer_pending, mut provider_only_pending) = (Amount::ZERO, 0_u32, 0_u32, 0_u32);
    for a in account_attempts(tx, key, unit)? {
        if holds_customer(&a) {
            ensure!(a.cycle == cycle, "unresolved customer obligation in old cycle");
            reserved = reserved.checked_add(a.reservation.upper_bound.unwrap_or(Amount::ZERO))?;
        }
        if a.state == ReservationState::InFlight { in_flight = in_flight.checked_add(1).context("count overflow")?; }
        if a.state == ReservationState::Pending { customer_pending = customer_pending.checked_add(1).context("count overflow")?; }
        else if a.state != ReservationState::InFlight && a.record.as_ref().is_none_or(|r| r.cost.evidence == EvidenceStatus::Pending) {
            provider_only_pending = provider_only_pending.checked_add(1).context("count overflow")?;
        }
    }
    let exposure = used.checked_add(reserved)?;
    let available = policy.limit.map(|limit| if exposure >= limit { Ok(Amount::ZERO) } else { limit.checked_sub(exposure) }).transpose()?;
    Ok(AccountView { key_id: key, policy, cycle, used, reserved, available, in_flight,
        pending: customer_pending.checked_add(provider_only_pending).context("count overflow")?, customer_pending, provider_only_pending })
}
fn account_attempts(tx: &Transaction<'_>, key: u64, unit: BillingUnit) -> anyhow::Result<Vec<AttemptView>> {
    let rows = tx.prepare("SELECT data FROM ledger_attempts WHERE key_id=?1 AND unit=?2 ORDER BY rowid")?
        .query_map(params![key.to_string(), unit_name(unit)], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
    rows.iter().map(|s| decode(s)).collect()
}
fn attempt_optional(tx: &Transaction<'_>, id: &str) -> anyhow::Result<Option<AttemptView>> {
    tx.query_row("SELECT data FROM ledger_attempts WHERE attempt_id=?1", [id], |r| r.get::<_, String>(0)).optional()?.map(|s| decode(&s)).transpose()
}
fn attempt(tx: &Transaction<'_>, id: &str) -> anyhow::Result<AttemptView> { attempt_optional(tx, id)?.context("unknown attempt") }
fn save_attempt(tx: &Transaction<'_>, a: &AttemptView) -> anyhow::Result<()> {
    tx.execute("UPDATE ledger_attempts SET data=?1 WHERE attempt_id=?2", params![json(a)?, a.reservation.attempt_id])?; Ok(())
}
fn request_optional(tx: &Transaction<'_>, id: &str) -> anyhow::Result<Option<RequestView>> {
    let row: Option<(String, String, Option<String>)> = tx.query_row("SELECT key_id,public_model,committed_attempt_id FROM ledger_requests WHERE request_id=?1", [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
    row.map(|(key, public_model, committed_attempt_id)| {
        let rows = tx.prepare("SELECT data FROM ledger_attempts WHERE request_id=?1 ORDER BY rowid")?
            .query_map([id], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        let event_rows=tx.prepare("SELECT event_id,kind,payload,created_at FROM ledger_events
            WHERE json_extract(payload,'$.attemptId') IN (SELECT attempt_id FROM ledger_attempts WHERE request_id=?1) ORDER BY rowid")?
            .query_map([id],|r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?
            .collect::<Result<Vec<_>,_>>()?;
        let events=event_rows.into_iter().map(|(event_id,kind,payload,created_at)| Ok(LedgerEvent {event_id,kind,payload:decode(&payload)?,created_at})).collect::<anyhow::Result<_>>()?;
        Ok(RequestView { request_id: id.into(), key_id: key.parse()?, public_model, committed_attempt_id, events,
            attempts: rows.iter().map(|s| decode(s)).collect::<anyhow::Result<_>>()? })
    }).transpose()
}
fn set_used(tx: &Transaction<'_>, a: &AccountView, used: Amount) -> anyhow::Result<()> {
    tx.execute("UPDATE ledger_accounts SET used=?1 WHERE key_id=?2 AND unit=?3", params![used.to_string(), a.key_id.to_string(), unit_name(a.policy.unit)])?; Ok(())
}
fn save_audit(tx: &Transaction<'_>, audit: AccountAuditView) -> anyhow::Result<()> {
    tx.execute("INSERT INTO ledger_account_audit(operation_id,key_id,data) VALUES(?1,?2,?3)",
        params![audit.operation_id,audit.key_id.to_string(),json(&audit)?])?;
    Ok(())
}
fn migration_complete(tx: &Transaction<'_>) -> anyhow::Result<bool> {
    let value: Option<String> = tx.query_row("SELECT value FROM ledger_metadata WHERE name='legacy_migration_complete'", [], |r| r.get(0)).optional()?;
    match value.as_deref() { None => Ok(false), Some("true") => Ok(true), _ => anyhow::bail!("invalid migration marker") }
}
fn event_matches(tx: &Transaction<'_>, id: &str, kind: &str, value: &impl Serialize) -> anyhow::Result<bool> {
    let old: Option<(String, String)> = tx.query_row("SELECT kind,payload FROM ledger_events WHERE event_id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    if let Some((old_kind, payload)) = old {
        ensure!(old_kind == kind && payload == json(value)?, "conflicting idempotency event"); Ok(true)
    } else { Ok(false) }
}
fn insert_event(tx: &Transaction<'_>, id: &str, kind: &str, value: &impl Serialize) -> anyhow::Result<()> {
    tx.execute("INSERT INTO ledger_events(event_id,kind,payload) VALUES(?1,?2,?3)", params![id, kind, json(value)?])?; Ok(())
}
fn holds_customer(a: &AttemptView) -> bool { matches!(a.state, ReservationState::InFlight | ReservationState::Pending) }
fn allowed(values: &[String], target: &str) -> bool { values.is_empty() || values.iter().any(|v| v == target) }
fn nonblank(value: &str) -> anyhow::Result<()> { ensure!(!value.trim().is_empty(), "identifier/reason cannot be blank"); Ok(()) }
fn unit_name(unit: BillingUnit) -> &'static str { match unit { BillingUnit::KiroCredit => "kiro_credit", BillingUnit::Cny => "CNY", BillingUnit::Usd => "USD" } }
fn json(value: &impl Serialize) -> anyhow::Result<String> { Ok(serde_json::to_string(value)?) }
fn decode<T: DeserializeOwned>(value: &str) -> anyhow::Result<T> { Ok(serde_json::from_str(value)?) }

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;
