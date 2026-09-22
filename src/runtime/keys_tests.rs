use super::*;
use crate::model::config::Config;

fn config_with(api: Option<&str>, admin: Option<&str>) -> Config {
    let mut config = Config::default();
    config.api_key = api.map(str::to_string);
    config.admin_api_key = admin.map(str::to_string);
    config
}

#[test]
fn both_keys_and_the_location_are_reported() {
    let lines = key_report_lines(
        &config_with(Some("sk-kiro-rs-abc"), Some("sk-admin-xyz")),
        Path::new("/data/kiro"),
    );
    let text = lines.join("\n");
    assert!(text.contains("sk-kiro-rs-abc"));
    assert!(text.contains("sk-admin-xyz"));
    assert!(text.contains("/data/kiro"));
    assert!(
        text.contains("/data/kiro/config.json"),
        "要指出配置文件本身"
    );
}

/// 缺失要说「未配置」，不能打一个空值让人以为程序坏了。
#[test]
fn a_missing_key_says_so_instead_of_printing_nothing() {
    let lines = key_report_lines(&config_with(None, Some("sk-admin-xyz")), Path::new("/d"));
    let text = lines.join("\n");
    assert!(text.contains("未配置"), "实际:\n{text}");
    assert!(text.contains("sk-admin-xyz"), "另一个还在就该照常给出");
}

/// 空字符串与空白串等同于未配置——配置里 `"apiKey": ""` 是常见的手滑，
/// 打一个空值出来只会让人以为密钥就是空的。
#[test]
fn a_blank_key_counts_as_missing() {
    for blank in ["", "   ", "\t"] {
        let lines = key_report_lines(&config_with(Some(blank), None), Path::new("/d"));
        assert!(
            lines.join("\n").contains("未配置"),
            "空白串 {blank:?} 应视为未配置"
        );
    }
}
