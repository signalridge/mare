-- ANSI & Base16 terminal-palette syntax themes.
-- Upstream: sharkdp/bat assets/themes/{ansi,base16}.tmTheme (MIT / Apache-2.0;
-- originally Template: Chris Kempson, Scheme: Mitchell Kember).
-- Modified for Mare: paired with ANSI alpha convention (#RRGGBB00/#00000001)
-- and trimmed to dark/light variant pairs; split out of bundled.lua.
return {
	{
		name = "ansi",
		label = "ANSI",
		doc = [[
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>name</key>
	<string>ansi</string>
	<key>settings</key>
	<array>
		<dict>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#00000001</string>
				<key>background</key>
				<string>#00000001</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>comment, punctuation.definition.comment</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#08000000</string>
				<key>fontStyle</key>
				<string>italic</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>keyword, storage, keyword.control</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#05000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>keyword.operator, punctuation.accessor</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#06000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>string, markup.inline.raw.string</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#02000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>string.regexp, constant.character.escape</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#06000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>constant.numeric, constant.language, constant.character, support.constant</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#03000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>variable.parameter</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#03000000</string>
				<key>fontStyle</key>
				<string>italic</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>variable.language, variable.other.constant</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#01000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>entity.name.function, support.function, meta.function-call</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#04000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>entity.name.type, entity.name.struct, entity.name.enum, entity.name.trait, support.type, support.class</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#03000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>entity.name.tag, meta.tag.sgml</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#01000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>entity.other.attribute-name</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#0B000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>invalid, invalid.deprecated</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#09000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.heading, entity.name.section</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#04000000</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.bold</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#00000001</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.italic</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#00000001</string>
				<key>fontStyle</key>
				<string>italic</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.underline, markup.underline.link</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#0C000000</string>
				<key>fontStyle</key>
				<string>underline</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.quote</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#08000000</string>
				<key>fontStyle</key>
				<string>italic</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.raw, markup.inline.raw</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#02000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.inserted, diff.inserted</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#0A000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.deleted, diff.deleted</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#09000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.changed, diff.changed</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#0B000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta.diff.range, meta.diff.header</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#0D000000</string>
			</dict>
		</dict>
	</array>
</dict>
</plist>
]],
	},
	{
		name = "base16",
		label = "Base16",
		doc = [[
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>name</key>
	<string>base16</string>
	<key>settings</key>
	<array>
		<dict>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#00000001</string>
				<key>background</key>
				<string>#00000001</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>comment, punctuation.definition.comment</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#08000000</string>
				<key>fontStyle</key>
				<string>italic</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>punctuation.definition, punctuation.separator, punctuation.terminator</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#07000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>keyword, storage, keyword.control</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#05000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>keyword.operator</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#07000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>string, markup.inline.raw.string</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#02000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>string.regexp, constant.character.escape</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#06000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>constant.numeric, constant.language, constant.character, support.constant</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#09000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>variable, variable.parameter</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#01000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>entity.name.function, support.function, meta.function-call</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#04000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>entity.name.type, entity.name.struct, entity.name.enum, entity.name.trait, support.type, support.class</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#03000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>entity.name.tag</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#01000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>entity.other.attribute-name</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#09000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>invalid, invalid.deprecated</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#01000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.heading, entity.name.section</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#04000000</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.bold</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#00000001</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.italic</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#00000001</string>
				<key>fontStyle</key>
				<string>italic</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.underline, markup.underline.link</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#06000000</string>
				<key>fontStyle</key>
				<string>underline</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.quote</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#08000000</string>
				<key>fontStyle</key>
				<string>italic</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.raw, markup.inline.raw</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#02000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.inserted, diff.inserted</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#02000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.deleted, diff.deleted</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#01000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>markup.changed, diff.changed</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#03000000</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta.diff.range, meta.diff.header</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#05000000</string>
			</dict>
		</dict>
	</array>
</dict>
</plist>
]],
	},
}
