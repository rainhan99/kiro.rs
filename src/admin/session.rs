//! 管理界面的会话。
//!
//! 登录用管理密码换一个有 TTL 的 token，之后的请求带 token 而不是密码。
//! 好处是密码不再长期躺在浏览器的 localStorage 里，而且过期能真的作废。
//!
//! **它的防护上限**：`config.json` 里的管理密码仍然是永久凭据，
//! 脚本与既有自动化还得用它（见 `admin_auth_middleware` 同时接受两者）。
//! 所以会话约束的是**浏览器那条路**——谁能读配置文件，谁就永远进得去。
//! 这句话要如实出现在界面上，不能让人以为加了会话就锁死了。
//!
//! 会话只存内存：重启即全部失效。对一个管理面来说这是对的——重启本来
//! 就该把人踢出去重新认一次。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use subtle::ConstantTimeEq;

/// token 长度。它等价于管理密码，短了就能爆破。
const TOKEN_LEN: usize = 48;

/// 一次签发的结果。
#[derive(Debug, Clone)]
pub struct IssuedSession {
    pub token: String,
    /// 到期时刻（秒级 Unix 时间戳）。`None` 表示不过期。
    pub expires_at: Option<i64>,
}

struct Entry {
    /// 最近一次被使用的时刻。滑动续期就是刷新它。
    touched: Instant,
}

/// 会话表。
pub struct SessionStore {
    inner: Mutex<Inner>,
}

struct Inner {
    entries: HashMap<String, Entry>,
    /// 0 表示不过期。
    ttl_hours: u64,
    /// 测试用的时间偏移。生产路径上恒为零。
    ///
    /// 没有它就只能靠 `sleep` 测过期，一个「1 小时后失效」的断言要跑
    /// 一小时——那种测试没人会跑，等于没有。
    skew: Duration,
}

impl SessionStore {
    pub fn new(ttl_hours: u64) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                ttl_hours,
                skew: Duration::ZERO,
            }),
        }
    }

    /// 签发一个新会话，顺手清扫过期条目。
    ///
    /// 在签发时清扫而不是起后台任务：登录是低频动作，清扫成本可忽略，
    /// 而少一个后台任务就少一处生命周期要管。
    pub fn issue(&self) -> IssuedSession {
        let mut inner = self.inner.lock();
        let now = Instant::now() + inner.skew;
        inner.sweep(now);

        let token = generate_token();
        inner.entries.insert(token.clone(), Entry { touched: now });

        let expires_at = inner.ttl().map(|ttl| {
            chrono::Utc::now().timestamp() + ttl.as_secs() as i64 - inner.skew.as_secs() as i64
        });
        IssuedSession { token, expires_at }
    }

    /// 校验并滑动续期。
    ///
    /// 滑动而不是绝对过期：绝对过期会让人在填一半表单时被登出，
    /// 而那正是最恼人的时刻。
    pub fn validate(&self, presented: &str) -> bool {
        if presented.is_empty() {
            return false;
        }
        let mut inner = self.inner.lock();
        let now = Instant::now() + inner.skew;

        // 常量时间比对：token 等价于管理密码。HashMap 查找本身是变时间的，
        // 所以逐个比——会话数量很少（一个管理面），代价可忽略。
        let ttl = inner.ttl();
        let hit = inner.entries.iter().find_map(|(token, entry)| {
            let matches: bool = token.as_bytes().ct_eq(presented.as_bytes()).into();
            let alive = ttl.is_none_or(|ttl| now.duration_since(entry.touched) <= ttl);
            (matches && alive).then(|| token.clone())
        });

        match hit {
            Some(token) => {
                if let Some(entry) = inner.entries.get_mut(&token) {
                    entry.touched = now;
                }
                true
            }
            None => false,
        }
    }

    /// 登出：立刻作废这一个 token，不影响别的会话。
    pub fn revoke(&self, presented: &str) {
        self.inner.lock().entries.remove(presented);
    }

    /// 改 TTL。**已有会话按新值算**——反过来会让「我把过期时间调短了」
    /// 这个动作在最需要它生效的那一刻不生效。
    pub fn set_ttl_hours(&self, hours: u64) {
        let mut inner = self.inner.lock();

        // **先用旧 TTL 清扫，再换新值**。顺序反了会有一个安静的洞：
        // 把过期关掉（TTL→0）时，新 TTL 下「不过期」，清扫成了空操作，
        // 于是已经死透的会话会连同一起复活。
        let now = Instant::now() + inner.skew;
        inner.sweep(now);

        inner.ttl_hours = hours;

        // 调短的情况：换完再清一次，免得超时的条目活到下一次 issue。
        inner.sweep(now);
    }

    pub fn ttl_hours(&self) -> u64 {
        self.inner.lock().ttl_hours
    }

    #[cfg(test)]
    pub fn advance_clock_for_test(&self, by: Duration) {
        self.inner.lock().skew += by;
    }

    #[cfg(test)]
    pub fn len_for_test(&self) -> usize {
        self.inner.lock().entries.len()
    }
}

impl Inner {
    fn ttl(&self) -> Option<Duration> {
        (self.ttl_hours > 0).then(|| Duration::from_secs(self.ttl_hours * 3600))
    }

    /// 清掉过期条目。
    ///
    /// 只拒绝不清扫的话，一个长跑的服务会随登录次数无限涨。
    fn sweep(&mut self, now: Instant) {
        let Some(ttl) = self.ttl() else {
            return;
        };
        self.entries
            .retain(|_, entry| now.duration_since(entry.touched) <= ttl);
    }
}

fn generate_token() -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    (0..TOKEN_LEN)
        .map(|_| CHARSET[fastrand::usize(..CHARSET.len())] as char)
        .collect()
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod session_tests;
