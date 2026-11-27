// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import sitemap from '@astrojs/sitemap';

const BASE = '/mare';
const SITE = 'https://signalridge.github.io';

const LATIN =
	'U+0000-00FF,U+0131,U+0152-0153,U+02BB-02BC,U+02C6,U+02DA,U+02DC,U+0304,U+0308,' +
	'U+0329,U+2000-206F,U+20AC,U+2122,U+2191,U+2193,U+2212,U+2215,U+FEFF,U+FFFD';
const LATIN_EXT =
	'U+0100-02BA,U+02BD-02C5,U+02C7-02CC,U+02CE-02D7,U+02DD-02FF,U+0304,U+0308,U+0329,' +
	'U+1D00-1DBF,U+1E00-1E9F,U+1EF2-1EFF,U+2020,U+20A0-20AB,U+20AD-20C0,U+2113,' +
	'U+2C60-2C7F,U+A720-A7FF';

/** @param {string} family @param {string} weight @param {string} file @param {string} range */
const face = (family, weight, file, range) =>
	`@font-face{font-family:"${family}";font-style:normal;font-weight:${weight};` +
	`font-display:swap;src:url("${BASE}/fonts/${file}") format("woff2");unicode-range:${range}}`;

/** @param {string} file */
const preload = (file) => ({
	tag: /** @type {const} */ ('link'),
	attrs: {
		rel: 'preload',
		href: `${BASE}/fonts/${file}`,
		as: 'font',
		type: 'font/woff2',
		// Not optional even though this is same-origin: fonts fetch in CORS mode,
		// and a preload without it is a wasted second request.
		crossorigin: 'anonymous',
	},
});

const head = [
	// Only the latin subsets of the text and display faces are preloaded;
	// latin-ext and the code face are left for the browser to fetch on demand.
	preload('inter-latin-wght-normal.woff2'),
	preload('space-grotesk-600-latin.woff2'),
	// All fonts are served from this origin. The faces are declared here rather
	// than in `custom.css` so Vite never tries to rewrite these `public/` URLs
	// (it warns on each one it cannot resolve at build time), and so they are
	// known while the HTML is still parsing instead of after the stylesheet
	// arrives.
	//
	// Inter (text) and JetBrains Mono (code, the face the TUI screenshots are
	// drawn in) are variable fonts; Space Grotesk (headings) is vendored at 600
	// only. Subsets and unicode-ranges are Google's own, so latin-ext is fetched
	// only by pages that need it. All three are SIL OFL 1.1; the licences are in
	// public/fonts/OFL-*.txt.
	{
		tag: /** @type {const} */ ('style'),
		content: [
			face('Inter Variable', '100 900', 'inter-latin-wght-normal.woff2', LATIN),
			face('Inter Variable', '100 900', 'inter-latin-ext-wght-normal.woff2', LATIN_EXT),
			face('JetBrains Mono Variable', '100 800', 'jetbrains-mono-latin-wght-normal.woff2', LATIN),
			face('Space Grotesk', '600', 'space-grotesk-600-latin.woff2', LATIN),
			face('Space Grotesk', '600', 'space-grotesk-600-latin-ext.woff2', LATIN_EXT),
			// Critical first-frame CSS: prevents raw unstyled SVG <path> blowout,
			// double dark/light wordmark flash, and white background FOUC before
			// external stylesheets arrive on slow or cold-cache connections.
			"html{background:#0e0f17;color:#f3f4fb}" +
			"html[data-theme='light']{background:#ffffff;color:#141624}" +
			"svg{max-width:100%;height:auto}" +
			".site-title img{height:22px;width:auto}" +
			"html:not([data-theme='light']) .dark\\:sl-hidden{display:none}" +
			"html[data-theme='light'] .light\\:sl-hidden{display:none}",
		].join(''),
	},
];

export default defineConfig({
	site: SITE,
	base: BASE,
	redirects: { '/': `${BASE}/en/` },
	integrations: [
		sitemap({
			filter: (page) => {
				const { pathname } = new URL(page);
				return pathname !== BASE && pathname !== `${BASE}/`;
			},
			i18n: {
				defaultLocale: 'en',
				locales: { en: 'en', zh: 'zh-CN', ja: 'ja' },
			},
		}),
		starlight({
			disable404Route: true,
			title: 'mare',
			description: 'A hyperextensible agent harness',
			logo: {
				dark: '../docs/assets/brand/mare-wordmark-nav-dark.svg',
				light: '../docs/assets/brand/mare-wordmark-nav-light.svg',
				replacesTitle: true,
			},
			favicon: '/mark.svg',
			components: {
				Header: './src/components/starlight/Header.astro',
				PageTitle: './src/components/starlight/PageTitle.astro',
				Footer: './src/components/starlight/Footer.astro',
			},
			customCss: ['./src/styles/custom.css'],
			head,
			// The TUI's skeleton is Catppuccin, so code on the site is too.
			expressiveCode: {
				themes: ['catppuccin-mocha', 'catppuccin-latte'],
				useStarlightUiThemeColors: true,
				styleOverrides: {
					borderRadius: '0.625rem',
					borderColor: 'var(--sl-color-hairline-light)',
					codeBackground: 'var(--mare-code-bg)',
					codeFontFamily: 'var(--__sl-font-mono)',
					codeFontSize: '0.8125rem',
					codeLineHeight: '1.7',
					codePaddingBlock: '1rem',
					codePaddingInline: '1.15rem',
					frames: {
						frameBoxShadowCssValue: 'none',
						editorActiveTabIndicatorTopColor: 'var(--sl-color-accent)',
						editorTabBarBackground: 'var(--mare-code-bg)',
						editorActiveTabBackground: 'var(--mare-code-bg)',
						terminalBackground: 'var(--mare-code-bg)',
						terminalTitlebarBackground: 'var(--mare-code-bg)',
						terminalTitlebarBorderBottomColor: 'var(--sl-color-hairline-light)',
					},
				},
			},
			social: [
				{ icon: 'github', label: 'GitHub', href: 'https://github.com/signalridge/mare' },
			],
			lastUpdated: true,
			defaultLocale: 'en',
			locales: {
				en: { label: 'English', lang: 'en' },
				zh: { label: '简体中文', lang: 'zh-CN' },
				ja: { label: '日本語', lang: 'ja' },
			},
			sidebar: [
				{
					label: 'Get started',
					translations: { 'zh-CN': '开始使用', ja: 'はじめに' },
					collapsed: false,
					items: [
						{ label: 'Overview', translations: { 'zh-CN': '概览', ja: '概要' }, slug: 'get-started/overview' },
						{ label: 'Installation', translations: { 'zh-CN': '安装', ja: 'インストール' }, slug: 'get-started/installation' },
						{ label: 'Quickstart', translations: { 'zh-CN': '快速上手', ja: 'クイックスタート' }, slug: 'get-started/quickstart' },
						{ label: 'Common workflows', translations: { 'zh-CN': '常见工作流', ja: 'よくあるワークフロー' }, slug: 'get-started/workflows' },
						{ label: 'Troubleshooting', translations: { 'zh-CN': '故障排查', ja: 'トラブルシューティング' }, slug: 'get-started/troubleshooting' },
					],
				},
				{
					label: 'Use mare',
					translations: { 'zh-CN': '使用 mare', ja: 'mare を使う' },
					collapsed: true,
					items: [
						{ label: 'The terminal UI', translations: { 'zh-CN': '终端界面', ja: 'ターミナル UI' }, slug: 'use/tui' },
						{ label: 'Built-in tools and shell', translations: { 'zh-CN': '内置工具与 Shell', ja: '組み込みツールとシェル' }, slug: 'use/tools' },
						{ label: 'Skills and prompt templates', translations: { 'zh-CN': '技能与提示词模板', ja: 'スキルとプロンプトテンプレート' }, slug: 'use/skills' },
						{ label: 'Subagents and side threads', translations: { 'zh-CN': '子代理与临时旁路对话', ja: 'サブエージェントとサイドスレッド' }, slug: 'use/subagents' },
						{ label: 'Sessions', translations: { 'zh-CN': '会话', ja: 'セッション' }, slug: 'use/sessions' },
						{ label: 'Checkpoints and review', translations: { 'zh-CN': '文件快照与代码审阅', ja: 'スナップショットとレビュー' }, slug: 'use/checkpoints-and-review' },
						{ label: 'Usage and compaction', translations: { 'zh-CN': '用量统计与上下文压缩', ja: '使用量とコンパクション' }, slug: 'use/usage-and-compaction' },
						{ label: 'Permissions and sandbox', translations: { 'zh-CN': '权限与沙箱', ja: '権限とサンドボックス' }, slug: 'use/permissions' },
						{ label: 'Models and providers', translations: { 'zh-CN': '模型与 provider', ja: 'モデルとプロバイダー' }, slug: 'use/models' },
						{ label: 'Context and instructions', translations: { 'zh-CN': '上下文与指令文件', ja: 'コンテキストと指示ファイル' }, slug: 'use/context' },
						{ label: 'MCP servers', translations: { 'zh-CN': 'MCP 服务器', ja: 'MCP サーバー' }, slug: 'use/mcp' },
						{ label: 'WebSocket and HTTP listener', translations: { 'zh-CN': '本地 WebSocket 与 HTTP 服务', ja: 'WebSocket と HTTP リスナー' }, slug: 'use/web' },
						{ label: 'Headless and CI', translations: { 'zh-CN': '无界面与 CI', ja: 'ヘッドレスと CI' }, slug: 'use/headless' },
					],
				},
				{
					label: 'Configure',
					translations: { 'zh-CN': '配置', ja: '設定' },
					collapsed: true,
					items: [
						{ label: 'Configuration files', translations: { 'zh-CN': '配置文件', ja: '設定ファイル' }, slug: 'configure/files' },
						{ label: 'Options', translations: { 'zh-CN': '选项', ja: 'オプション' }, slug: 'configure/options' },
						{ label: 'Key bindings', translations: { 'zh-CN': '键位', ja: 'キーバインド' }, slug: 'configure/keymaps' },
						{ label: 'Appearance', translations: { 'zh-CN': '外观', ja: '外観' }, slug: 'configure/ui' },
						{ label: 'Hooks, status line, and notify', translations: { 'zh-CN': '钩子、状态栏与通知', ja: 'フック・ステータスライン・通知' }, slug: 'configure/hooks' },
						{ label: 'Packages', translations: { 'zh-CN': '包', ja: 'パッケージ' }, slug: 'configure/packages' },
					],
				},
				{
					label: 'Extend',
					translations: { 'zh-CN': '扩展', ja: '拡張' },
					collapsed: true,
					items: [
						{ label: 'Plugins', translations: { 'zh-CN': '插件', ja: 'プラグイン' }, slug: 'extend/plugins' },
						{ label: 'Writing a plugin', translations: { 'zh-CN': '编写插件', ja: 'プラグインを書く' }, slug: 'extend/writing-a-plugin' },
						{ label: 'Services and events', translations: { 'zh-CN': '服务与事件', ja: 'サービスとイベント' }, slug: 'extend/services-and-events' },
						{ label: 'Runtime guarantees', translations: { 'zh-CN': '插件运行时承诺', ja: 'ランタイム保証' }, slug: 'extend/runtime-guarantees' },
						{ label: 'Lua API', translations: { 'zh-CN': 'Lua API', ja: 'Lua API' }, slug: 'extend/lua-api' },
						{ label: 'Host kernel', translations: { 'zh-CN': '宿主内核与 task', ja: 'ホストカーネルと task' }, slug: 'extend/host-kernel' },
						{ label: 'Host loader', translations: { 'zh-CN': '宿主加载器', ja: 'ホストローダー' }, slug: 'extend/host-loader' },
					],
				},
				{
					label: 'Integrate',
					translations: { 'zh-CN': '集成', ja: '連携' },
					collapsed: true,
					items: [
						{ label: 'JSON-RPC', translations: { 'zh-CN': 'JSON-RPC', ja: 'JSON-RPC' }, slug: 'integrate/rpc' },
						{ label: 'Agent Client Protocol', translations: { 'zh-CN': 'Agent Client Protocol', ja: 'Agent Client Protocol' }, slug: 'integrate/acp' },
						{ label: 'JSON event stream', translations: { 'zh-CN': 'JSON 事件流', ja: 'JSON イベントストリーム' }, slug: 'integrate/json-stream' },
					],
				},
				{
					label: 'Reference',
					translations: { 'zh-CN': '参考', ja: 'リファレンス' },
					collapsed: true,
					items: [
						{ label: 'CLI', translations: { 'zh-CN': '命令行', ja: 'CLI' }, slug: 'reference/cli' },
						{ label: 'Slash commands', translations: { 'zh-CN': '斜杠命令', ja: 'スラッシュコマンド' }, slug: 'reference/slash-commands' },
						{ label: 'RPC reference', translations: { 'zh-CN': 'RPC 参考', ja: 'RPC リファレンス' }, slug: 'reference/rpc' },
						{ label: 'Events', translations: { 'zh-CN': '事件', ja: 'イベント' }, slug: 'reference/events' },
						{ label: 'Environment variables', translations: { 'zh-CN': '环境变量', ja: '環境変数' }, slug: 'reference/environment' },
						{ label: 'Files and directories', translations: { 'zh-CN': '文件与目录', ja: 'ファイルとディレクトリ' }, slug: 'reference/files' },
						{ label: 'Exit codes', translations: { 'zh-CN': '退出码', ja: '終了コード' }, slug: 'reference/exit-codes' },
					],
				},
				{
					label: 'Ecosystem',
					translations: { 'zh-CN': '生态', ja: 'エコシステム' },
					collapsed: true,
					items: [
						{
							label: 'mare-plugins',
							link: 'https://github.com/signalridge/mare-plugins',
							attrs: { target: '_blank', rel: 'noopener noreferrer' },
						},
						{
							label: 'cordis-lua',
							link: 'https://github.com/signalridge/cordis-lua',
							attrs: { target: '_blank', rel: 'noopener noreferrer' },
						},
						{
							label: 'llms.txt',
							link: `${SITE}${BASE}/llms.txt`,
							attrs: { target: '_blank', rel: 'noopener noreferrer' },
						},
					],
				},
			],
		}),
	],
});
