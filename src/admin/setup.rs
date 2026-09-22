//! 首次初始化：把管理权从「随机生成的一串东西」交到用户手里。
//!
//! 为什么不是「启动时生成一个随机 adminApiKey，用户自己去翻」：
//! 那串东西用户既没设过也记不住，最后要么去翻配置文件，要么干脆不换。
//!
//! 为什么不是「首次打开就让人设密码」：服务端绑 `0.0.0.0`，这等于把管理权
//! 送给第一个扫到端口的人。所以认领管理员必须出示一个**只有能看到控制台的人
//! 才拿得到**的凭证——这就是 setup token，与 Jenkins / Gitea 同一个模式。
//!
//! token 只存内存、不落盘：落盘就多一个会被备份、被打包、被 `scp` 走的秘密。
//! 代价是错过控制台输出要重启——所以未初始化期间**每次**启动都打印，
//! 不是只打第一次。

use parking_lot::RwLock;
use subtle::ConstantTimeEq;

/// setup token 的长度。够长到不可爆破，又还能从终端里手抄。
const TOKEN_LEN: usize = 40;

/// 用户自设密码的最短长度。
///
/// 这不是密码学强度要求——它守的是「随手敲个 1 就过」。真正的门是
/// setup token；密码是之后每天要用的东西，太短纯粹是给自己挖坑。
pub const MIN_ADMIN_KEY_LEN: usize = 8;

/// 初始化状态。
///
/// 「已初始化」= 配置里有非空的 `adminApiKey`。判据只有这一条，
/// 不另存标志位——标志位与实际凭据会漂移，而漂移的后果是把人锁在门外。
pub struct SetupState {
    /// 未初始化时的一次性凭证。已初始化则为 `None`，**不生成**。
    token: RwLock<Option<String>>,
}

impl SetupState {
    /// 依据配置里有没有 `adminApiKey` 决定要不要发 token。
    ///
    /// 已有的部署一律视为已初始化——不能因为引入这个特性就把现有用户
    /// 锁在门外。
    pub fn from_configured_key(admin_api_key: Option<&str>) -> Self {
        let initialized = admin_api_key.map(str::trim).is_some_and(|k| !k.is_empty());
        Self {
            token: RwLock::new((!initialized).then(generate_token)),
        }
    }

    /// 是否已初始化。
    pub fn initialized(&self) -> bool {
        self.token.read().is_none()
    }

    /// 当前的 setup token，仅未初始化时存在。
    pub fn token(&self) -> Option<String> {
        self.token.read().clone()
    }

    /// 校验 token 并作废它。
    ///
    /// 常量时间比对：token 是能换管理权的东西，逐字节短路比较会把它的
    /// 前缀泄露给计时攻击。
    ///
    /// 成功后立刻置 `None`——「设完即关」不是文档里的承诺，是这一行。
    pub fn consume(&self, presented: &str) -> bool {
        let mut guard = self.token.write();
        let Some(expected) = guard.as_deref() else {
            // 已初始化。即使 token 正确也不能再用——这里根本没有 token 了。
            return false;
        };
        let matches: bool = expected.as_bytes().ct_eq(presented.as_bytes()).into();
        if matches {
            *guard = None;
        }
        matches
    }
}

fn generate_token() -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    (0..TOKEN_LEN)
        .map(|_| CHARSET[fastrand::usize(..CHARSET.len())] as char)
        .collect()
}

/// 校验用户要设的管理密码。
pub fn validate_admin_key(key: &str) -> Result<String, String> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err("密码不能为空".to_string());
    }
    if trimmed.chars().count() < MIN_ADMIN_KEY_LEN {
        return Err(format!("密码至少 {MIN_ADMIN_KEY_LEN} 个字符"));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
#[path = "setup_tests.rs"]
mod setup_tests;
