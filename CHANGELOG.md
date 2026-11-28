# Changelog

All notable changes to mare will be documented in this file.

## [1.1.0](https://github.com/signalridge/mare/compare/v1.0.0...v1.1.0) (2026-11-28)


### Features

* **acp:** return structured busy data for occupied session restore ([#386](https://github.com/signalridge/mare/issues/386)) ([d72e475](https://github.com/signalridge/mare/commit/d72e475b8f8dd1ea930673722ed6865b8a9b4366))
* **evals:** add interleaved Harbor A/B runner, negative probes, and offline re-scoring ([#174](https://github.com/signalridge/mare/issues/174)) ([b3a8851](https://github.com/signalridge/mare/commit/b3a8851e6f482c1ccb3f2c45a230968cb56de36b))
* **llm:** add data-tier provider factories for anthropic and google wires ([#15](https://github.com/signalridge/mare/issues/15)) ([b98632f](https://github.com/signalridge/mare/commit/b98632fdb7ba5f15dc1b0fbb7a04cca63b9e80fb))
* **usage:** track and display reported reasoning tokens in /usage ([#158](https://github.com/signalridge/mare/issues/158)) ([fadcfaa](https://github.com/signalridge/mare/commit/fadcfaa55a9aea88ffa06dcee568a24d7d154761))


### Bug Fixes

* **acp:** emit config_option_update on model/effort changes, keep resource URIs and image blobs, and forward tool-result images ([#69](https://github.com/signalridge/mare/issues/69)) ([472358f](https://github.com/signalridge/mare/commit/472358f3802592f49db9764f81f9674eaa6396f1))
* **acp:** return logical RFC 3339 updatedAt in session/list ([#171](https://github.com/signalridge/mare/issues/171)) ([645e4d6](https://github.com/signalridge/mare/commit/645e4d6a02015a21108896d64a33ed331871fd94))
* **acp:** validate recorded additionalDirectories on restore and paginate session/list with keyset cursor ([#67](https://github.com/signalridge/mare/issues/67)) ([3a1f99b](https://github.com/signalridge/mare/commit/3a1f99bb10419e753c742088f30e688af3cddc44))
* **binding:** coalesce in-process crashguard state I/O with per-request deadlines ([#78](https://github.com/signalridge/mare/issues/78)) ([36d79de](https://github.com/signalridge/mare/commit/36d79de26c4af2c0514dc7def0d6aa5563b2bc68))
* **cli:** resolve CLI, headless, doctor, and RPC --clean edge cases ([#64](https://github.com/signalridge/mare/issues/64)) ([07a98c5](https://github.com/signalridge/mare/commit/07a98c5d799687766d8e365f33689e7e704df0fb))
* **compact:** keep head and tail of failed tool results when trimming summary input ([#377](https://github.com/signalridge/mare/issues/377)) ([57c612a](https://github.com/signalridge/mare/commit/57c612a5ad0ce3eae6d17587b962c66a6119c46f))
* **compact:** scale default headroom_tokens for small context windows ([#139](https://github.com/signalridge/mare/issues/139)) ([3d8caa9](https://github.com/signalridge/mare/commit/3d8caa9422717e7a8d41d359b87d8207725e476b))
* **config:** strip leading UTF-8 BOM in file-backed JSON readers ([#163](https://github.com/signalridge/mare/issues/163)) ([d4c5120](https://github.com/signalridge/mare/commit/d4c51203e2224791fb02817a302e2d9dd7944d1d))
* **conversations:** allow /side and /btw on empty sessions without parent history ([#62](https://github.com/signalridge/mare/issues/62)) ([58efa52](https://github.com/signalridge/mare/commit/58efa52c45d77af33bf801ea7240c43505fce283))
* **conversations:** preserve /side failure backfill while external editor is open ([#59](https://github.com/signalridge/mare/issues/59)) ([8623205](https://github.com/signalridge/mare/commit/8623205668776a96406720804143fdae3341310a))
* **conversations:** render side-conversation answers once by clearing partial stream state on commit ([#61](https://github.com/signalridge/mare/issues/61)) ([332fb6d](https://github.com/signalridge/mare/commit/332fb6df258a09a84d141cdc92b57767e2303708))
* **docker:** run container entrypoint under tini to reap zombies ([#142](https://github.com/signalridge/mare/issues/142)) ([c3f270c](https://github.com/signalridge/mare/commit/c3f270c3a62297a792da4a576ff5283bd55e098e))
* **http,llm:** bound SSE wait for first provider event despite keepalive comments ([#134](https://github.com/signalridge/mare/issues/134)) ([cdac4c5](https://github.com/signalridge/mare/commit/cdac4c5ac64901c692e11be833634441e851cf16))
* **install:** replace in-use mare.exe safely in install.ps1 ([#131](https://github.com/signalridge/mare/issues/131)) ([d0783f7](https://github.com/signalridge/mare/commit/d0783f76d91895d0331c9a53695f2108988bac26))
* **llm:** treat reasoning-only or whitespace-only stop replies as empty responses ([#138](https://github.com/signalridge/mare/issues/138)) ([9f5b636](https://github.com/signalridge/mare/commit/9f5b636c8ecdc8ddde249b07df90ee4700a9233e))
* **mcp:** list tools even when capabilities.tools is omitted and show 0-tools hint in /mcp ([#70](https://github.com/signalridge/mare/issues/70)) ([338ae95](https://github.com/signalridge/mare/commit/338ae9538bd84c4ad4bcacf566780f4ac15a03d8))
* **mcp:** order MCP tool declarations deterministically across connection order ([#378](https://github.com/signalridge/mare/issues/378)) ([7463720](https://github.com/signalridge/mare/commit/74637204c01ec93a5956ed4f6ef3d3f22dd5a001))
* **mcp:** restrict Streamable HTTP redirects to same origin ([#132](https://github.com/signalridge/mare/issues/132)) ([70a3d02](https://github.com/signalridge/mare/commit/70a3d02996ebd1a896c2be0d0048c64689646c3d))
* **net:** honor net.fake_ip on open-network web_fetch requests ([#167](https://github.com/signalridge/mare/issues/167)) ([ef1fe6d](https://github.com/signalridge/mare/commit/ef1fe6d3134af0be469aa935e49f549af0c6238d))
* **nix:** wrap mare with fallback git on PATH for plugin sync ([#141](https://github.com/signalridge/mare/issues/141)) ([a84f488](https://github.com/signalridge/mare/commit/a84f488d58e1d57631c1f2f3e1a13088c219725d))
* **permissions,sandbox:** preserve and apply approved .git write grants across calls and backends ([#17](https://github.com/signalridge/mare/issues/17)) ([1ba5c78](https://github.com/signalridge/mare/commit/1ba5c78e3d0342f30c1a80594e65ee1f1d5b8bac))
* **permissions,store:** resolve concurrent rule reload and index update races ([#102](https://github.com/signalridge/mare/issues/102)) ([1679231](https://github.com/signalridge/mare/commit/167923189182d9294f872766534a5bd2d89fd3c7)), closes [#29](https://github.com/signalridge/mare/issues/29)
* **permissions:** align subagent inheritance, steer resync, and parent preset approvals ([#10](https://github.com/signalridge/mare/issues/10)) ([b83f919](https://github.com/signalridge/mare/commit/b83f919ee1e763c1a709b21907dd5e69a6226e54))
* **permissions:** open preset picker on Shift+Tab in untrusted folders ([#8](https://github.com/signalridge/mare/issues/8)) ([138e6a8](https://github.com/signalridge/mare/commit/138e6a8774a3206eea2a89e26d7caa763c7932da)), closes [#20](https://github.com/signalridge/mare/issues/20)
* **plugin:** serialize concurrent runtime directory extraction with a cross-process lock ([#147](https://github.com/signalridge/mare/issues/147)) ([6949571](https://github.com/signalridge/mare/commit/694957117c84a0b0390df539440d49503ee71aea))
* **process:** avoid double-signalling leader group on polite Unix tree signals ([#16](https://github.com/signalridge/mare/issues/16)) ([6f8b597](https://github.com/signalridge/mare/commit/6f8b59733025f891f1fd70ce9f1da25b5a23e55d))
* **provider-openai-compat:** preserve refusal text and map refusal finish reasons ([#155](https://github.com/signalridge/mare/issues/155)) ([1716d28](https://github.com/signalridge/mare/commit/1716d284fe3b9c6df8b3d4e92cdab02154192542))
* **providers:** classify model_context_window_exceeded stop reason as context_overflow ([#332](https://github.com/signalridge/mare/issues/332)) ([4bf95f5](https://github.com/signalridge/mare/commit/4bf95f5db866c57cdeedcbea64e1cb6b414a9178))
* **providers:** emit empty properties map for bare object tools on OpenAI Chat wire ([#340](https://github.com/signalridge/mare/issues/340)) ([f769714](https://github.com/signalridge/mare/commit/f7697144fd01f663ffbcc5417100d50b1e631094))
* **providers:** persist plaintext reasoning content from completed OpenAI Responses items ([#359](https://github.com/signalridge/mare/issues/359)) ([618543f](https://github.com/signalridge/mare/commit/618543f55f442ce5ee564d228b869bcc528867fc))
* **release:** build static Linux musl release binaries ([#63](https://github.com/signalridge/mare/issues/63)) ([981d5b9](https://github.com/signalridge/mare/commit/981d5b9e2244e824b1ac7701bc73c62f89d7c9d8))
* **reload:** return structured errors and clean busy notices without tracebacks ([#119](https://github.com/signalridge/mare/issues/119)) ([df340fe](https://github.com/signalridge/mare/commit/df340fea608df0edbee48644a2aa37ae1b254180)), closes [#57](https://github.com/signalridge/mare/issues/57)
* **rewind:** share parent snapshot journal with subagents ([#72](https://github.com/signalridge/mare/issues/72)) ([61d0963](https://github.com/signalridge/mare/commit/61d0963f84e562795a6397154dbd7de34ad26331))
* **sandbox:** align sandbox, proxy, and preset config handling ([#80](https://github.com/signalridge/mare/issues/80)) ([cb72a97](https://github.com/signalridge/mare/commit/cb72a9790a9731ce2ccf34392cb25144adbfc806))
* **session:** keep last_msg nil for non-message leaf entries in snapshot_context ([#130](https://github.com/signalridge/mare/issues/130)) ([012eaaf](https://github.com/signalridge/mare/commit/012eaafe15b1c0dcff5558288a05f6e910ee44c3))
* **style:** preserve chromatic contrast in ANSI-16 fallback palette ([#349](https://github.com/signalridge/mare/issues/349)) ([544ae49](https://github.com/signalridge/mare/commit/544ae49c44ad5f1619ea471d3eaf6d6d0e4ce3b4))
* **subagent:** inherit parent workspace trust state and live model ([#73](https://github.com/signalridge/mare/issues/73)) ([9c013f8](https://github.com/signalridge/mare/commit/9c013f894d0f56a18b78b7b33b15a3b820b27bed))
* **subagent:** treat empty optional tool overrides as omitted ([#162](https://github.com/signalridge/mare/issues/162)) ([55a549e](https://github.com/signalridge/mare/commit/55a549efe39956fc4523d0eb44bb4834e6746186))
* **trust:** scope CLI trust flags to anchor workspace and preserve undecided trust sources in RPC and ACP ([#68](https://github.com/signalridge/mare/issues/68)) ([6b02dce](https://github.com/signalridge/mare/commit/6b02dce52f30661c7a53a2b87af1b89785c5da21))
* **tui:** account for inner editor borders in Modal::Input height budget ([#152](https://github.com/signalridge/mare/issues/152)) ([e28a4e1](https://github.com/signalridge/mare/commit/e28a4e1f9633b6a99009a82dc85f696ea05d46e4))
* **tui:** buffer trailing lone ESC across reads in live input decoder ([#58](https://github.com/signalridge/mare/issues/58)) ([65b737e](https://github.com/signalridge/mare/commit/65b737efc90519ededee9e53a0fed30380b9d6b4))
* **tui:** clarify subagent approval modal details and waiting status ([#66](https://github.com/signalridge/mare/issues/66)) ([7474f52](https://github.com/signalridge/mare/commit/7474f5287b333721ae8b1591a6d1f11711511b24))
* **tui:** enable DEC 2031 theme-change notifications and refresh background ([#352](https://github.com/signalridge/mare/issues/352)) ([a2885a3](https://github.com/signalridge/mare/commit/a2885a3b0898597426c474e33ea44bf75f93ed88))
* **tui:** erase main live area before entering alt-screen transcript overlay ([#76](https://github.com/signalridge/mare/issues/76)) ([6ef74a3](https://github.com/signalridge/mare/commit/6ef74a3b7e05c66161b26cedbd6e0675e445d1bf))
* **tui:** fix Markdown fenced code block regex panic and pre-validate syntaxes ([#65](https://github.com/signalridge/mare/issues/65)) ([f94ca4e](https://github.com/signalridge/mare/commit/f94ca4eca141fd5aa242ef6776946a3b7784b34a))
* **tui:** fold multi-file apply_patch diffs across files with filename headers ([#77](https://github.com/signalridge/mare/issues/77)) ([5b6fe54](https://github.com/signalridge/mare/commit/5b6fe540e82b9450f5bab6d91b99e15a42b27ca5))
* **tui:** keep focus unknown until terminal reports FocusIn or FocusOut ([#387](https://github.com/signalridge/mare/issues/387)) ([bf5bf8f](https://github.com/signalridge/mare/commit/bf5bf8f0e216d81c9f517f9c47bf3e00d3570928))
* **tui:** re-query terminal background via OSC 11 after resuming from suspend ([#339](https://github.com/signalridge/mare/issues/339)) ([5859b1d](https://github.com/signalridge/mare/commit/5859b1dcca1ce011c35bf7d84b90bbdda5c6598b))
* **tui:** restore reviewed combined bullet glyph as default working indicator ([#376](https://github.com/signalridge/mare/issues/376)) ([13b1920](https://github.com/signalridge/mare/commit/13b192082c9cad2ecea1ec680db19404ee117ab8))
* **tui:** toggle Ctrl+O compact/expanded view in place across committed scrollback ([#75](https://github.com/signalridge/mare/issues/75)) ([fe0cad0](https://github.com/signalridge/mare/commit/fe0cad0f78a4eb2099f35cb8dc675e40d677835d))
* **ui:** clear and redraw welcome banner on session/new ([#79](https://github.com/signalridge/mare/issues/79)) ([40e683c](https://github.com/signalridge/mare/commit/40e683cc177f28e56001c82abc390209ed55248d))
* **ui:** show retry status on working line and clamp toast borders ([#83](https://github.com/signalridge/mare/issues/83)) ([8502deb](https://github.com/signalridge/mare/commit/8502debf428a7d60171bde882044fde58842a939))


### Dependencies

* **rust:** bump rusqlite from 0.42.0 to 0.43.0 ([#7](https://github.com/signalridge/mare/issues/7)) ([19c624c](https://github.com/signalridge/mare/commit/19c624c6b9f8645af7b0c7d05ff3ed48ad6c9aa6))

## 1.0.0 (2026-11-26)

### Features

* Initial public release (v1.0.0).
