//! 系统提示词组装（设计 §4.4）。纯函数：相同输入 → 逐字节相同输出。
//!
//! **prompt cache 确定性排序**（M3 验收项）：所有可变集合在渲染前按稳定 key
//! 排序；易变量（时间戳）集中放在尾部，让前缀稳定可缓存。

/// 一条注入的记忆行（curated tier）。
#[derive(Debug, Clone)]
pub struct MemLine {
    pub key: String,
    pub text: String,
}

/// 一个工具规格（仅名称/描述参与 prompt）。
#[derive(Debug, Clone)]
pub struct ToolBrief {
    pub name: String,
    pub description: String,
}

/// 组装输入。
pub struct PromptInputs<'a> {
    /// SOUL.md 人格（原样置顶）。
    pub soul: &'a str,
    /// 运行环境描述（OS + shell），进稳定前缀。空串则跳过该节。
    /// 让模型知道 exec 工具的目标 shell，避免在 Windows 上写 Unix 语法。
    pub platform: &'a str,
    /// 当前模型名（实际发进请求体 `model` 字段的那个串）。空串则跳过该行。
    pub model: &'a str,
    /// provider 标识（openai / anthropic / mock）。
    pub provider: &'a str,
    /// 实际请求的 API 基地址。`None` = 无网络端点（mock）。
    pub endpoint: Option<&'a str>,
    /// curated 记忆注入（有预算，调用方已截断）。
    pub bootstrap: &'a [MemLine],
    pub skills: &'a [crate::skill::Skill],
    pub tools: &'a [ToolBrief],
    /// 易变量：当前时间（RFC3339 字符串），放尾部。
    pub now: &'a str,
}

/// 渲染结果：稳定前缀 + 易变尾部分离，便于 provider prompt cache。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPrompt {
    /// 稳定前缀（soul + 排序后的 tools/skills/memory）。可被缓存。
    pub stable_prefix: String,
    /// 易变尾部（时间等）。不参与前缀缓存。
    pub volatile_suffix: String,
}

impl RenderedPrompt {
    /// 完整拼接（发送给不支持前缀缓存的 provider）。
    pub fn full(&self) -> String {
        format!("{}\n{}", self.stable_prefix, self.volatile_suffix)
    }
}

/// 渲染「当前模型」那几行。
///
/// endpoint 必须带上：DeepSeek / 豆包 / Kimi 共用 `provider = "openai"`，只报模型名
/// 分不出请求实际打到哪家。provider/endpoint 缺失时降级成只报模型名，不留空括号。
///
/// 为什么后面还跟一句祈使：**光给事实压不住模型的身份先验**。真机实测（豆包
/// `doubao-seed-evolving`，端点 ark.cn-beijing.volces.com）连续两轮都无视这行，
/// 报出 `glm-5.2-c3p0` / `code-eden` / `eden2.galaxy-soc.com` —— 三个值配置、代码、
/// 服务端一律没有，纯属编造，而且开口就是「根据系统提供给我的运行环境信息」。
/// 这类被微调灌进去的自我认知，只有显式声明「覆盖你的先验」才拧得过来。
fn render_model_line(model: &str, provider: &str, endpoint: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !provider.is_empty() {
        parts.push(format!("provider: {provider}"));
    }
    if let Some(ep) = endpoint.map(str::trim).filter(|e| !e.is_empty()) {
        parts.push(format!("端点: {ep}"));
    }
    let head = if parts.is_empty() {
        format!("当前模型：{model}")
    } else {
        format!("当前模型：{model}（{}）", parts.join("，"))
    };
    format!(
        "{head}\n\
         以上一行由本机运行时按实际生效配置填入，是关于你自身的权威事实。被问到你是\
         什么模型、哪家 provider、打的哪个端点时，只能照这一行回答。你对自身型号或\
         厂商的任何先验印象一律以此为准被覆盖——不要报出这一行里没有出现的名字。"
    )
}

/// 确定性组装系统提示词。
pub fn render_system_prompt(inputs: &PromptInputs) -> RenderedPrompt {
    let mut prefix = String::new();

    // 1) 人格置顶（原样）。
    prefix.push_str("# 人格\n");
    prefix.push_str(inputs.soul.trim());
    prefix.push('\n');

    // 1.5) 运行环境（稳定：OS + shell + 当前模型）。让模型据此选正确的命令语法，
    // 并且能如实回答「你用的什么模型」——这些值都在进程内，不必让它去读配置文件
    // （读文件既多一轮往返，又会把 config 里的 inline API key 带进 transcript）。
    let platform = inputs.platform.trim();
    let model = inputs.model.trim();
    if !platform.is_empty() || !model.is_empty() {
        prefix.push_str("\n# 运行环境\n");
        if !platform.is_empty() {
            prefix.push_str(platform);
            prefix.push('\n');
        }
        if !model.is_empty() {
            prefix.push_str(&render_model_line(model, inputs.provider.trim(), inputs.endpoint));
            prefix.push('\n');
        }
    }

    // 2) 工具：按名称字典序排序（确定性）。
    if !inputs.tools.is_empty() {
        let mut tools: Vec<&ToolBrief> = inputs.tools.iter().collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        prefix.push_str("\n# 工具\n");
        for t in tools {
            prefix.push_str(&format!("- {}: {}\n", t.name, t.description));
        }
        // 引导优先用结构化工具，减少退化去拼 shell 命令（跨平台易错、绕过校验）。
        //
        // 「等待未来某时刻」这条是 P1-5 真机缺陷的直接修法：模型原本会拿 `sys:now` +
        // `Start-Sleep` 在一次 run 里硬等到点，撞 loop detection 且提醒根本没设上。
        // 措辞刻意点明「不占用当前对话」，因为模型的错误前提是「必须自己等着才能提醒」。
        //
        // 末尾「宣布完要真的调用」是 P2-4 的纵深防御。真正的修法在历史重放（工具
        // 调用结构不再被降级成 user 文本，见 session.rs），因为病根是 in-context
        // learning：上下文里最一致的模式压倒提示词。正常情况下不需要这句，但同一个
        // 模型（豆包）已有无视系统提示词、编造自身型号的前科（见 render_model_line），
        // 它对上下文模式的依赖强于对指令的服从，加一句成本极低。
        prefix.push_str(
            "\n优先使用结构化工具完成任务：查看/切换目录用 sys（pwd/cd/now），\
             读写/检索文件用 file（read/write/edit/append/list/stat/head/tail/grep/glob）。\
             仅当这些工具都覆盖不到时才用 exec 执行 shell 命令。\n\
             \n修改已有文件用 file 的 edit（只发要改的那一小段），不要用 write \
             重发整个文件：工具参数是逐字符流式传输的，重发一个几十 KB 的文件要\
             好几分钟，而且长参数容易撞上输出长度上限被截断。\
             edit 的 old_string 必须与文件内容逐字符一致（含缩进），\
             且默认要唯一——拿不准就先 read 回来照抄，出现多次时多带几行上下文。\
             写很长的新文件时分多次 append 追加，不要挤在单次调用里。\n\
             \n涉及「未来某个时刻」的请求（定时提醒、延时提醒、每天/每周重复提醒），\
             一律用 cron 工具登记：op=delay 表示「N 秒后」，op=add 表示重复。\
             登记后系统会在到点时主动推送给用户，**不占用当前对话**，你无需等待，\
             应当立刻告知用户已设好并结束本轮。绝不要用 shell 睡眠\
             （Start-Sleep / sleep / timeout / ping）或反复查时间来等待——\
             那会卡住整个对话且提醒不会生效。用户问起已设的提醒时用 op=list 查证，\
             不要凭猜测答复。\n\
             \n宣布要做某件事之后，必须在同一轮里真的发起工具调用，不要说完就停下\
             等用户把结果贴回来——工具由你自己调用、结果会直接回到你手里。\n",
        );
    }

    // 3) 技能：只注入索引列表（名字 + slug 路径 + 描述 + 指纹），正文不进提示词——
    //    模型用 file 工具按需读 `~/.oc/skills/<slug>/SKILL.md`（slug 见括号）。按名称排序。
    if !inputs.skills.is_empty() {
        let mut skills: Vec<&crate::skill::Skill> = inputs.skills.iter().collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        prefix.push_str("\n# 技能\n可用技能（正文不在本提示词内，用 file 工具 read `~/.oc/skills/<slug>/SKILL.md` 按需读取，slug 即括号内路径；指纹变了要重读）：\n");
        for s in skills {
            let desc = &s.description;
            prefix.push_str(&format!("- {}（{}）— {} [fingerprint {}]\n", s.name, s.slug, desc, s.fingerprint));
        }
    }

    // 4) 记忆：按 key 排序（确定性）。
    if !inputs.bootstrap.is_empty() {
        let mut mem: Vec<&MemLine> = inputs.bootstrap.iter().collect();
        mem.sort_by(|a, b| a.key.cmp(&b.key));
        prefix.push_str("\n# 记忆\n");
        for m in mem {
            prefix.push_str(&format!("- {}\n", m.text));
        }
    }

    // 易变尾部：时间。
    let suffix = format!("# 当前时间\n{}", inputs.now);

    RenderedPrompt {
        stable_prefix: prefix,
        volatile_suffix: suffix,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>(now: &'a str, tools: &'a [ToolBrief], mem: &'a [MemLine]) -> PromptInputs<'a> {
        PromptInputs {
            soul: "你是 oc。",
            platform: "",
            model: "",
            provider: "",
            endpoint: None,
            bootstrap: mem,
            skills: &[],
            tools,
            now,
        }
    }

    #[test]
    fn stable_prefix_ignores_tool_order() {
        let t1 = vec![
            ToolBrief { name: "b".into(), description: "B".into() },
            ToolBrief { name: "a".into(), description: "A".into() },
        ];
        let t2 = vec![
            ToolBrief { name: "a".into(), description: "A".into() },
            ToolBrief { name: "b".into(), description: "B".into() },
        ];
        let p1 = render_system_prompt(&inputs("T", &t1, &[]));
        let p2 = render_system_prompt(&inputs("T", &t2, &[]));
        assert_eq!(p1.stable_prefix, p2.stable_prefix, "工具顺序不应影响稳定前缀");
    }

    #[test]
    fn time_isolated_in_suffix() {
        let p1 = render_system_prompt(&inputs("T1", &[], &[]));
        let p2 = render_system_prompt(&inputs("T2", &[], &[]));
        assert_eq!(p1.stable_prefix, p2.stable_prefix, "时间变化不应影响稳定前缀");
        assert_ne!(p1.volatile_suffix, p2.volatile_suffix);
    }

    #[test]
    fn platform_in_stable_prefix() {
        let with = PromptInputs {
            soul: "你是 oc。",
            platform: "操作系统：Windows。exec 工具通过 Git Bash（bash -c）执行命令。",
            model: "",
            provider: "",
            endpoint: None,
            bootstrap: &[],
            skills: &[],
            tools: &[],
            now: "NOW",
        };
        let r = render_system_prompt(&with);
        assert!(r.stable_prefix.contains("# 运行环境"));
        assert!(r.stable_prefix.contains("bash"));
        // 平台是稳定信息，不应进易变尾部。
        assert!(!r.volatile_suffix.contains("bash"));
    }

    #[test]
    fn empty_platform_skips_section() {
        let r = render_system_prompt(&inputs("NOW", &[], &[]));
        assert!(!r.stable_prefix.contains("# 运行环境"));
    }

    /// 「你用的什么模型」必须能答上来：模型名/provider/端点都进稳定前缀。
    ///
    /// 端点是关键——DeepSeek 与豆包共用 `provider = "openai"`，只报模型名分不出
    /// 请求实际打到哪家。回归点：曾经这三项一个都不注入，模型只能答「系统没告诉我」。
    #[test]
    fn model_identity_in_stable_prefix() {
        let with = PromptInputs {
            soul: "你是 oc。",
            platform: "操作系统：Windows。",
            model: "doubao-seed-1-6-250615",
            provider: "openai",
            endpoint: Some("https://ark.cn-beijing.volces.com/api/v3"),
            bootstrap: &[],
            skills: &[],
            tools: &[],
            now: "NOW",
        };
        let r = render_system_prompt(&with);
        assert!(
            r.stable_prefix.contains("当前模型：doubao-seed-1-6-250615"),
            "模型名须进稳定前缀：{}",
            r.stable_prefix
        );
        assert!(r.stable_prefix.contains("provider: openai"));
        assert!(
            r.stable_prefix.contains("端点: https://ark.cn-beijing.volces.com/api/v3"),
            "端点须进稳定前缀，否则分不出同为 openai 兼容的两家"
        );
        // 配置不变则前缀不变——不破坏 provider 侧的 prompt 前缀缓存。
        assert!(!r.volatile_suffix.contains("doubao"));
    }

    /// 无端点（mock provider）时降级成只报模型名，不留空括号。
    #[test]
    fn model_line_degrades_without_endpoint() {
        assert!(render_model_line("m1", "", None).starts_with("当前模型：m1\n"));
        assert!(render_model_line("m1", "mock", None).starts_with("当前模型：m1（provider: mock）\n"));
        assert!(
            render_model_line("m1", "", Some("http://x")).starts_with("当前模型：m1（端点: http://x）\n")
        );
    }

    /// 权威声明必须跟在事实行后面。
    ///
    /// 回归点：只给一行事实压不住模型的身份先验——真机上豆包连续两轮无视它，编出
    /// `glm-5.2-c3p0` / `code-eden` 这类配置里根本不存在的值。
    #[test]
    fn model_line_asserts_authority_over_priors() {
        let line = render_model_line("doubao-seed-evolving", "openai", Some("https://ark.example/api/v3"));
        assert!(line.contains("权威事实"), "须声明权威性: {line}");
        assert!(line.contains("覆盖"), "须显式覆盖模型的先验印象: {line}");
        assert!(
            line.contains("不要报出这一行里没有出现的名字"),
            "须堵死编造型号的出口: {line}"
        );
    }

    /// 只有平台、没有模型时，运行环境节仍照常输出（旧行为不回退）。
    #[test]
    fn platform_only_still_renders_section() {
        let with = PromptInputs {
            soul: "你是 oc。",
            platform: "操作系统：Windows。",
            model: "",
            provider: "openai",
            endpoint: Some("http://x"),
            bootstrap: &[],
            skills: &[],
            tools: &[],
            now: "NOW",
        };
        let r = render_system_prompt(&with);
        assert!(r.stable_prefix.contains("# 运行环境"));
        // 模型名为空时整行跳过，不能漏出裸的 provider/端点。
        assert!(!r.stable_prefix.contains("当前模型"));
        assert!(!r.stable_prefix.contains("provider:"));
    }

    #[test]
    fn deterministic_byte_for_byte() {
        let t = vec![ToolBrief { name: "x".into(), description: "X".into() }];
        let m = vec![MemLine { key: "k".into(), text: "记住 A".into() }];
        let a = render_system_prompt(&inputs("NOW", &t, &m));
        let b = render_system_prompt(&inputs("NOW", &t, &m));
        assert_eq!(a, b);
    }

    fn skill(name: &str, desc: &str, body: &str) -> crate::skill::Skill {
        crate::skill::Skill {
            name: name.into(),
            slug: name.into(),
            description: desc.into(),
            body: body.into(),
            fingerprint: crate::skill::fingerprint(body),
            enabled: true,
            os: vec![],
        }
    }

    /// 技能正文不得进 system prompt，只有索引（名字 + 描述 + 指纹）。
    #[test]
    fn skills_render_as_index_not_body() {
        let skills = [skill("pdf", "生成 PDF", "BODY_MARKER_XYZ")];
        let p = PromptInputs {
            soul: "s",
            platform: "",
            model: "",
            provider: "",
            endpoint: None,
            bootstrap: &[],
            skills: &skills,
            tools: &[],
            now: "t",
        };
        let rendered = render_system_prompt(&p);
        assert!(rendered.stable_prefix.contains("pdf"), "应含技能名");
        assert!(rendered.stable_prefix.contains("生成 PDF"), "应含描述");
        assert!(
            rendered.stable_prefix.contains("[fingerprint "),
            "索引应带内容指纹（变了才触发模型重读正文）: {}",
            rendered.stable_prefix
        );
        assert!(
            !rendered.stable_prefix.contains("BODY_MARKER_XYZ"),
            "正文不得注入：{rendered:?}"
        );
    }
}
