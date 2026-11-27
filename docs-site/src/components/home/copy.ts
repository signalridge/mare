// Text for the standalone home pages (src/pages/[lang]/index.astro).
// Everything here describes shipped behaviour; the docs under docs/<lang>/ are
// the source for each claim. Code samples live in Home.astro.

export type Lang = 'en' | 'zh' | 'ja';
export const LANGS: Lang[] = ['en', 'zh', 'ja'];

export const HTML_LANG: Record<Lang, string> = {
	en: 'en',
	zh: 'zh-CN',
	ja: 'ja',
};
export const LANG_LABEL: Record<Lang, string> = {
	en: 'English',
	zh: '简体中文',
	ja: '日本語',
};

interface SectionLink {
	title: string;
	body: string;
	link: string;
	more: string;
}

export interface Copy {
	title: string;
	description: string;
	nav: {
		aria: string;
		skip: string;
		architecture: string;
		features: string;
		install: string;
		docs: string;
		language: string;
		theme: string;
		themes: [string, string, string];
	};
	hero: {
		title: [string, string];
		lede: string;
		start: string;
		copyCmd: string;
		copied: string;
		video: string;
		pause: string;
		play: string;
		caption: string;
	};
	architecture: {
		title: string;
		lede: string;
		videoLabel: string;
		videoCaption: string;
		diagramCaption: string;
		layers: [
			{ name: string; lang: string; desc: string },
			{ name: string; lang: string; desc: string },
			{ name: string; lang: string; desc: string },
		];
		more: string;
	};
	features: {
		title: string;
		lede: string;
		plugins: SectionLink;
		subagents: SectionLink;
		sessions: SectionLink;
		sandbox: SectionLink;
		providers: SectionLink;
		integrate: SectionLink;
		startup: SectionLink;
	};
	sessionRows: [string, string, string, string];
	sandboxPresets: [
		{ id: 'read-only'; flag: string; summary: string },
		{ id: 'auto'; flag: string; summary: string },
		{ id: 'full-access'; flag: string; summary: string },
	];
	effortNote: string;
	copyCode: string;
	copiedCode: string;
	install: {
		title: string;
		lede: string;
		tabsAria: string;
		detected: string;
		copy: string;
		copied: string;
		script: string;
		firstRunTitle: string;
		firstRunBody: string;
		firstRunComment: string;
		firstRunMore: string;
		downloads: string;
		downloadsLede: string;
		archive: string;
		note: string;
		guide: string;
		container: string;
		apple: string;
		intel: string;
		latest: string;
	};
	ecosystem: {
		title: string;
		plugins: string;
		cordis: string;
		llms: string;
	};
	footer: {
		tagline: string;
		docs: string;
		releases: string;
		changelog: string;
		license: string;
		name: string;
	};
}

export const COPY: Record<Lang, Copy> = {
	en: {
		title: 'mare: a hyperextensible agent harness',
		description:
			'mare is a hyperextensible agent harness built as a small Rust kernel plus Lua 5.5 plugins. Rust handles isolation, storage, and terminal rendering; Lua plugins implement the agent loop, tools, providers, and UI components.',
		nav: {
			aria: 'Primary navigation',
			skip: 'Skip to content',
			architecture: 'Architecture',
			features: 'Capabilities',
			install: 'Install',
			docs: 'Docs',
			language: 'Language',
			theme: 'Theme',
			themes: ['Theme: system', 'Theme: light', 'Theme: dark'],
		},
		hero: {
			title: ['A hyperextensible', 'agent harness.'],
			lede: 'A small Rust kernel confines processes, persists sessions, renders the terminal UI, and runs Lua 5.5 plugins. Read, wrap, or replace everything above it.',
			start: 'Get started',
			copyCmd: 'Copy install command',
			copied: 'Copied',
			video: 'A mare session: the agent reads a file, edits it and shows the diff',
			pause: 'Pause recording',
			play: 'Play recording',
			caption: 'Recorded from the real terminal UI against an offline scripted model.',
		},
		architecture: {
			title: 'Rust isolation, Lua plugins.',
			lede: 'Rust handles process isolation, session storage, and terminal rendering. Lua plugins implement the agent loop, tools, providers, compaction, and UI components.',
			videoLabel: 'mare architecture animation: Rust kernel, Cordis service bus, Lua plugins and front ends',
			videoCaption: 'How requests flow across the Rust kernel, Cordis service bus, and Lua plugins.',
			diagramCaption: 'Rust handles process isolation, session storage, and terminal rendering; Lua plugins implement the agent loop, tools, and UI components.',
			layers: [
				{
					name: 'Rust core',
					lang: 'Native binary',
					desc: 'OS sandboxing, durable JSONL storage, file snapshots, project trust, watchdog, and terminal renderer.',
				},
				{
					name: 'Built-in plugins',
					lang: 'Lua 5.5',
					desc: 'Agent loop, tools, model providers, compaction, permissions, sessions, review, and MCP client.',
				},
				{
					name: 'Your plugins',
					lang: 'Lua 5.5',
					desc: 'Extend or replace any built-in with override = true from init.lua, user plugins, or packages.',
				},
			],
			more: 'Architecture overview',
		},
		features: {
			title: 'Plugins',
			lede: 'Built-in and user plugins communicate through Cordis services and events.',
			plugins: {
				title: 'Inspect and replace built-in plugins.',
				body: 'Inspect built-in sources with mare runtime, override any plugin by name with override = true, and apply changes live with /reload.',
				link: 'en/extend/plugins/',
				more: 'Plugin model',
			},
			subagents: {
				title: 'Isolated child agents and side threads.',
				body: 'Subagents run with their own session context; they inherit the parent working directory unless cwd is set, with per-type or per-call tools and model. Define types in Markdown and inspect them in /agents.',
				link: 'en/use/subagents/',
				more: 'Subagents',
			},
			sessions: {
				title: 'Fork, branch, and rewind turns.',
				body: 'Messages and step commits are persisted to append-only JSONL, and file tools snapshot originals before editing so code and history revert together.',
				link: 'en/use/sessions/',
				more: 'Sessions',
			},
			sandbox: {
				title: 'OS-level process confinement.',
				body: 'Commands run under bubblewrap or Landlock on Linux, Seatbelt on macOS, or restricted Windows tokens. Untrusted projects skip project code.',
				link: 'en/use/permissions/',
				more: 'Permissions and sandbox',
			},
			providers: {
				title: 'Unified effort control across providers.',
				body: 'Use a ChatGPT account, API keys, or any OpenAI-compatible endpoint. Set reasoning effort per model on the CLI or inside a session.',
				link: 'en/use/models/',
				more: 'Models and providers',
			},
			integrate: {
				title: 'One engine, five front ends.',
				body: 'Run headless prompts or JSON streams in CI, connect editors via ACP, or drive sessions over mare RPC 3.0 (JSON-RPC 2.0) and WebSocket.',
				link: 'en/integrate/rpc/',
				more: 'RPC and integrations',
			},
			startup: {
				title: 'Single binary, measurable startup.',
				body: 'Built-in plugins ship compiled into one binary. Profile boot stages with --profile-startup or start without user config via --clean.',
				link: 'en/reference/cli/',
				more: 'CLI reference',
			},
		},
		sessionRows: [
			'branch from your latest message',
			'branch from any earlier point',
			'restore files to a turn',
			'continue the last session',
		],
		sandboxPresets: [
			{ id: 'read-only', flag: 'mare --permission read-only', summary: 'Read files freely; ask before writing or leaving the sandbox.' },
			{ id: 'auto', flag: 'mare --permission auto', summary: 'Read files your user can read; write inside the workspace and temp directories. Asks before expanding write access.' },
			{ id: 'full-access', flag: 'mare --permission full-access', summary: 'Disable confinement for trusted automation (--yolo).' },
		],
		effortNote: 'Set on the CLI or switch inside a session:',
		copyCode: 'Copy',
		copiedCode: 'Copied',
		install: {
			title: 'Install and run.',
			lede: 'Select a platform. The install script verifies the release archive against checksums.txt.',
			tabsAria: 'Operating system',
			detected: 'Detected',
			copy: 'Copy',
			copied: 'Copied',
			script: 'Install script',
			firstRunTitle: 'First run.',
			firstRunBody: 'Install, cd into a repository, and run mare. When onboarding is enabled and no provider credentials are available, the TUI guides login and default model selection.',
			firstRunComment: 'first run: sign in and pick a model',
			firstRunMore: 'Quickstart',
			downloads: 'Release archives',
			downloadsLede: 'Prebuilt archives for six targets, each with SHA-256 checksums.',
			archive: 'archive',
			note: 'Releases also include .deb and .rpm packages, checksums.txt, Cosign signatures, and build attestations.',
			guide: 'Installation guide',
			container: 'Container',
			apple: 'Apple silicon',
			intel: 'Intel',
			latest: 'Latest',
		},
		ecosystem: {
			title: 'Related repositories.',
			plugins: 'Optional plugins, command hooks, and themes installed via mare.plugins.add.',
			cordis: 'Standalone Lua 5.5 Cordis plugin kernel used by mare.',
			llms: 'Full English documentation exported as plain text.',
		},
		footer: {
			tagline: 'A hyperextensible agent harness.',
			docs: 'Documentation',
			releases: 'Releases',
			changelog: 'Changelog',
			license: 'Apache-2.0',
			name: 'Named after the lunar maria: the core is the moon, plugins are the seas across its surface.',
		},
	},
	zh: {
		title: 'mare：可高度扩展的 agent harness',
		description:
			'mare 是可高度扩展的 agent harness：Rust 负责进程隔离、会话存储和终端渲染，Lua 5.5 插件实现 agent 循环、工具、provider 与界面组件。',
		nav: {
			aria: '主导航',
			skip: '跳到正文',
			architecture: '架构',
			features: '能力',
			install: '安装',
			docs: '文档',
			language: '语言',
			theme: '主题',
			themes: ['主题：跟随系统', '主题：浅色', '主题：深色'],
		},
		hero: {
			title: ['可高度扩展的', 'agent harness。'],
			lede: '小巧的 Rust 内核负责隔离进程、持久保存会话、渲染终端界面并运行 Lua 5.5 插件；其余一切皆可阅读、包装或替换。',
			start: '开始使用',
			copyCmd: '复制安装命令',
			copied: '已复制',
			video: 'mare 会话录屏：agent 读取文件、修改并展示 diff',
			pause: '暂停录屏',
			play: '播放录屏',
			caption: '录制自真实终端界面，使用离线脚本模型。',
		},
		architecture: {
			title: 'Rust 负责隔离，Lua 扩展能力。',
			lede: 'Rust 负责进程隔离、会话存储和终端渲染；Lua 插件实现 agent 循环、工具、provider、上下文压缩和界面组件。',
			videoLabel: 'mare 架构动画：Rust 内核、Cordis 服务总线、Lua 插件与多前端',
			videoCaption: '请求如何在 Rust 内核、Cordis 服务总线与 Lua 插件之间流转。',
			diagramCaption: 'Rust 负责进程隔离、会话存储和终端渲染；Lua 插件实现 agent 循环、工具和界面组件。',
			layers: [
				{
					name: 'Rust 核心',
					lang: '原生二进制',
					desc: 'OS 沙箱、崩溃安全 JSONL 存储、文件快照、项目信任、看门狗与终端渲染器。',
				},
				{
					name: '内置插件',
					lang: 'Lua 5.5',
					desc: 'agent 循环、内置工具、模型 provider、上下文压缩、权限审批、会话、审阅与 MCP 客户端。',
				},
				{
					name: '你的插件',
					lang: 'Lua 5.5',
					desc: '通过 init.lua、用户目录或插件包扩展，用 override = true 替换任意内置插件。',
				},
			],
			more: '架构概览',
		},
		features: {
			title: '插件',
			lede: '内置插件和用户插件通过 Cordis 服务与事件通信。',
			plugins: {
				title: '查看和替换内置插件。',
				body: '用 mare runtime 查看内置源码，用 override = true 按名称替换任意插件，再用 /reload 即时生效。',
				link: 'zh/extend/plugins/',
				more: '插件模型',
			},
			subagents: {
				title: '隔离的子代理与旁路线程。',
				body: '子代理有独立会话上下文；默认沿用父工作目录，可通过 cwd 指定目录，并按类型或调用参数选择工具和模型。用 Markdown 定义类型，在 /agents 查看状态。',
				link: 'zh/use/subagents/',
				more: '子代理',
			},
			sessions: {
				title: '分叉、分支与按轮回退。',
				body: '消息与 step 提交以追加式 JSONL 持久化，文件工具在写入前保存快照，代码与历史可一起回退。',
				link: 'zh/use/sessions/',
				more: '会话',
			},
			sandbox: {
				title: '操作系统层进程隔离。',
				body: 'Linux 使用 bubblewrap 或 Landlock，macOS 使用 Seatbelt，Windows 使用受限令牌；未信任项目不加载项目代码。',
				link: 'zh/use/permissions/',
				more: '权限与沙箱',
			},
			providers: {
				title: '跨 provider 统一推理强度。',
				body: '支持 ChatGPT 账号、API key 或 OpenAI 兼容端点，可按模型在命令行或会话内设置推理强度。',
				link: 'zh/use/models/',
				more: '模型与 provider',
			},
			integrate: {
				title: '一套引擎，五种前端入口。',
				body: '在 CI 中运行无界面提示或 JSON 事件流，通过 ACP 接入编辑器，或使用 mare RPC 3.0（JSON-RPC 2.0）与 WebSocket 驱动会话。',
				link: 'zh/integrate/rpc/',
				more: 'RPC 与集成',
			},
			startup: {
				title: '单一原生二进制，启动可测。',
				body: '内置插件随单一二进制发布。用 --profile-startup 查看各阶段耗时，用 --clean 跳过个人配置启动。',
				link: 'zh/reference/cli/',
				more: '命令行参考',
			},
		},
		sessionRows: [
			'从最近一条消息分叉',
			'从任意更早的位置分支',
			'把文件恢复到某一轮',
			'继续上一次会话',
		],
		sandboxPresets: [
			{ id: 'read-only', flag: 'mare --permission read-only', summary: '自由读取文件；写入或离开沙箱前一律询问。' },
			{ id: 'auto', flag: 'mare --permission auto', summary: '默认可读当前用户能读取的文件，可写工作区和临时目录；扩大写权限时请求批准。' },
			{ id: 'full-access', flag: 'mare --permission full-access', summary: '关闭沙箱约束，适合受信任自动化（--yolo）。' },
		],
		effortNote: '在命令行指定或在会话内切换：',
		copyCode: '复制',
		copiedCode: '已复制',
		install: {
			title: '安装并运行。',
			lede: '选择系统。安装脚本会下载最新版本并按 checksums.txt 校验。',
			tabsAria: '操作系统',
			detected: '当前系统',
			copy: '复制',
			copied: '已复制',
			script: '安装脚本',
			firstRunTitle: '首次运行。',
			firstRunBody: '安装后进入项目目录运行 mare。启用了引导且没有可用的 provider 凭据时，TUI 会引导登录并选择默认模型。',
			firstRunComment: '首次运行：登录并选择模型',
			firstRunMore: '快速上手',
			downloads: '发布压缩包',
			downloadsLede: '6 个目标平台的预编译压缩包，均附 SHA-256 校验值。',
			archive: '压缩包',
			note: '发布页同时提供 .deb、.rpm、checksums.txt、Cosign 签名与构建证明。',
			guide: '安装指南',
			container: '容器',
			apple: 'Apple 芯片',
			intel: 'Intel',
			latest: '最新',
		},
		ecosystem: {
			title: '相关仓库。',
			plugins: '可通过 mare.plugins.add 安装的可选插件、命令钩子和主题。',
			cordis: 'mare 使用的独立 Lua 5.5 Cordis 插件内核。',
			llms: '导出为纯文本的完整英文文档。',
		},
		footer: {
			tagline: '可高度扩展的 agent harness。',
			docs: '文档',
			releases: '发布',
			changelog: '更新日志',
			license: 'Apache-2.0',
			name: '名字来自月海（lunar maria）：内核是月亮，插件是它表面的海。',
		},
	},
	ja: {
		title: 'mare: 高度に拡張できるエージェントハーネス',
		description:
			'mare は高度に拡張できるエージェントハーネスです。Rust がプロセス隔離・セッション保存・ターミナル描画を担い、Lua 5.5 プラグインがエージェントループ、ツール、プロバイダー、UI コンポーネントを実装します。',
		nav: {
			aria: 'メインナビゲーション',
			skip: '本文へ移動',
			architecture: 'アーキテクチャ',
			features: '機能',
			install: 'インストール',
			docs: 'ドキュメント',
			language: '言語',
			theme: 'テーマ',
			themes: ['テーマ: システム', 'テーマ: ライト', 'テーマ: ダーク'],
		},
		hero: {
			title: ['高度に拡張できる', 'エージェントハーネス。'],
			lede: '小さな Rust カーネルがプロセスを隔離し、セッションを保存し、ターミナル UI を描画し、Lua 5.5 プラグインを動かします。上のすべてを読んで置き換えられます。',
			start: 'はじめる',
			copyCmd: 'インストールコマンドをコピー',
			copied: 'コピー済み',
			video: 'mare のセッション: エージェントがファイルを読み、編集し、差分を表示する',
			pause: '録画を一時停止',
			play: '録画を再生',
			caption: '実際のターミナル UI をオフラインのスクリプトモデルで録画しています。',
		},
		architecture: {
			title: 'Rust による隔離、Lua による拡張。',
			lede: 'Rust がプロセス隔離・セッション保存・ターミナル描画を担い、Lua プラグインがエージェントループ・ツール・プロバイダー・コンパクション・UI コンポーネントを実装します。',
			videoLabel: 'mare アーキテクチャアニメーション: Rust カーネル、Cordis サービスバス、Lua プラグインとフロントエンド',
			videoCaption: 'Rust カーネル、Cordis サービスバス、Lua プラグイン間のリクエストの流れ。',
			diagramCaption: 'Rust がプロセス隔離・セッション保存・ターミナル描画を担い、Lua プラグインがエージェントループ・ツール・UI コンポーネントを実装します。',
			layers: [
				{
					name: 'Rust コア',
					lang: 'ネイティブ単一バイナリ',
					desc: 'OS サンドボックス、耐クラッシュ JSONL 保存、ファイルスナップショット、信頼管理、TUI レンダラー。',
				},
				{
					name: '組み込みプラグイン',
					lang: 'Lua 5.5',
					desc: 'エージェントループ、ツール、プロバイダー、コンパクション、権限、セッション、レビュー、MCP クライアント。',
				},
				{
					name: 'あなたのプラグイン',
					lang: 'Lua 5.5',
					desc: 'init.lua やパッケージから拡張し、override = true でどの組み込みも置き換え可能。',
				},
			],
			more: 'アーキテクチャ概要',
		},
		features: {
			title: 'プラグイン',
			lede: '組み込みプラグインとユーザープラグインは Cordis のサービスとイベントで連携します。',
			plugins: {
				title: '組み込みプラグインの確認と置き換え。',
				body: 'mare runtime で組み込みソースを確認し、override = true で名前指定の上書き、/reload で反映します。',
				link: 'ja/extend/plugins/',
				more: 'プラグインモデル',
			},
			subagents: {
				title: '独立した子エージェントとサイドスレッド。',
				body: 'サブエージェントは独自のセッションコンテキストを持ち、既定で親の作業ディレクトリを引き継ぎます（cwd で指定可能）。ツールとモデルは種別または呼び出し引数で選び、Markdown で定義して /agents で確認します。',
				link: 'ja/use/subagents/',
				more: 'サブエージェント',
			},
			sessions: {
				title: 'フォーク・分岐・ターン巻き戻し。',
				body: 'メッセージとステップの確定状態を追記型 JSONL に永続化し、ファイルツールは編集前スナップショットを残すため、コードと履歴を一緒に巻き戻せます。',
				link: 'ja/use/sessions/',
				more: 'セッション',
			},
			sandbox: {
				title: 'OS レイヤーでのプロセス隔離。',
				body: 'Linux の bubblewrap / Landlock、macOS の Seatbelt、Windows の制限付きトークンで保護。未信頼のプロジェクトコードは読み込みません。',
				link: 'ja/use/permissions/',
				more: '権限とサンドボックス',
			},
			providers: {
				title: 'プロバイダー横断の effort 制御。',
				body: 'ChatGPT アカウント、API キー、OpenAI 互換エンドポイントに対応。モデルごとの effort を CLI またはセッション内で指定します。',
				link: 'ja/use/models/',
				more: 'モデルとプロバイダー',
			},
			integrate: {
				title: '1 つのエンジン、5 つのフロントエンド。',
				body: 'CI 向けのヘッドレス実行や JSON ストリーム、エディター向けの ACP、mare RPC 3.0（JSON-RPC 2.0）と WebSocket に対応します。',
				link: 'ja/integrate/rpc/',
				more: 'RPC と連携',
			},
			startup: {
				title: '単一バイナリ、計測できる起動時間。',
				body: '組み込みプラグインは単一バイナリに同梱。--profile-startup で段階別に計測し、--clean で個人設定なし起動ができます。',
				link: 'ja/reference/cli/',
				more: 'CLI リファレンス',
			},
		},
		sessionRows: [
			'最新のメッセージからフォーク',
			'過去の任意の地点から分岐',
			'ファイルをあるターンに戻す',
			'前回のセッションを続ける',
		],
		sandboxPresets: [
			{ id: 'read-only', flag: 'mare --permission read-only', summary: '読み取りは自由。書き込みやサンドボックス外へのアクセス前に確認。' },
			{ id: 'auto', flag: 'mare --permission auto', summary: '現在のユーザーが読めるファイルは読み取り可。ワークスペースと一時ディレクトリへの書き込みを許可し、書き込み範囲の拡大前に確認します。' },
			{ id: 'full-access', flag: 'mare --permission full-access', summary: '信頼された自動化向けに制限を解除（--yolo）。' },
		],
		effortNote: '起動時の指定またはセッション内での切り替え:',
		copyCode: 'コピー',
		copiedCode: 'コピー済み',
		install: {
			title: 'インストールして実行。',
			lede: 'プラットフォームを選択します。スクリプトは最新リリースを取得し checksums.txt で検証します。',
			tabsAria: 'オペレーティングシステム',
			detected: '検出済み',
			copy: 'コピー',
			copied: 'コピー済み',
			script: 'インストールスクリプト',
			firstRunTitle: '初回実行。',
			firstRunBody: 'インストール後、リポジトリへ移動して mare を実行します。オンボーディングが有効で利用可能なプロバイダー認証情報がない場合、TUI がログインとモデル選択を案内します。',
			firstRunComment: '初回: サインインしてモデルを選ぶ',
			firstRunMore: 'クイックスタート',
			downloads: 'リリースアーカイブ',
			downloadsLede: '6 ターゲット向けのビルド済みアーカイブと SHA-256。',
			archive: 'アーカイブ',
			note: '.deb / .rpm パッケージ、checksums.txt、Cosign 署名、ビルド証明も同梱。',
			guide: 'インストール手順',
			container: 'コンテナ',
			apple: 'Apple シリコン',
			intel: 'Intel',
			latest: '最新',
		},
		ecosystem: {
			title: '関連リポジトリ。',
			plugins: 'mare.plugins.add で導入できる任意プラグイン、コマンドフック、テーマ。',
			cordis: 'mare が使う単体利用可能な Lua 5.5 プラグインカーネル。',
			llms: 'プレーンテキスト化した英語ドキュメント全文。',
		},
		footer: {
			tagline: '高度に拡張できるエージェントハーネス。',
			docs: 'ドキュメント',
			releases: 'リリース',
			changelog: '変更履歴',
			license: 'Apache-2.0',
			name: '名前は月の海（lunar maria）から。コアが月で、プラグインはその表面に広がる海です。',
		},
	},
};
