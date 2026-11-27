-- Inspired GitHub bundled syntax theme (XML part 2/2 and theme table).
-- Upstream: Seth Lopez (sethlopez/InspiredGitHub.tmtheme, MIT),
-- distributed via syntect 5.3.0 (MIT).
-- Modified for Mare: added Mare UI palette table and combined XML parts.
return {
	name = "inspired-github",
	label = "Inspired GitHub",
	base = "light",
	palette = {
		accent = "#0086b3",
		accent2 = "#795da3",
		text = "#323232",
		muted = "#5c5e5c",
		dim = "#767676",
		border = "#c8c8c8",
		selection = "#d9edf7",
		code_bg = "#f5f5f5",
		green = "#3c763d",
		yellow = "#8a6d3b",
		red = "#a71d5d",
		blue = "#0086b3",
	},
	syntax = require("theme-default.bundled_inspired_github_a")
		.. "\n"
		.. [[
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta.structure.dictionary.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#183691</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>meta.structure.dictionary.json meta.structure.dictionary.value.json string.quoted.double.json</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string></string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>source.python keyword</string>
			<key>settings</key>
			<dict>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>source.python storage</string>
			<key>settings</key>
			<dict>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>source.python storage.type</string>
			<key>settings</key>
			<dict>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>source.python entity.name.function</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>source.php entity.name.type.class</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>variable.language.ruby</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#ED6A43</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>entity.name.type.module.ruby</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#795DA3</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>entity.name.type.class.ruby</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#795DA3</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>entity.other.inherited-class.ruby</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#795DA3</string>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>text.html.markdown punctuation.definition</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#A71D5D</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>text.html.markdown meta.separator</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#B3B3B3</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>text.html.markdown markup.heading</string>
			<key>settings</key>
			<dict>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>text.html.markdown markup.raw.block</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>text.html.markdown markup.raw.inline</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#323232</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>text.html.markdown meta.link, text.html.markdown meta.image</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#4183C4</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>text.html.markdown markup.underline.link, text.html.markdown constant.other.reference</string>
			<key>settings</key>
			<dict>
				<key>fontStyle</key>
				<string>italic</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>text.html.markdown markup.list</string>
			<key>settings</key>
			<dict>
				<key>foreground</key>
				<string>#ED6A43</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>text.html.markdown markup.bold</string>
			<key>settings</key>
			<dict>
				<key>fontStyle</key>
				<string>bold</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>text.html.markdown markup.italic</string>
			<key>settings</key>
			<dict>
				<key>fontStyle</key>
				<string>italic</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>text.html.markdown markup.bold markup.italic</string>
			<key>settings</key>
			<dict>
				<key>fontStyle</key>
				<string>bold italic</string>
			</dict>
		</dict>
		<dict>
			<key>scope</key>
			<string>text.html.markdown markup.italic markup.bold</string>
			<key>settings</key>
			<dict>
				<key>fontStyle</key>
				<string>bold italic</string>
			</dict>
		</dict>
	</array>
</dict>
</plist>
]],
}
