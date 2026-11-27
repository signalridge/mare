-- Model-named bundled syntax themes (ada, babbage, curie, cushman, dali, davinci).
return {
	{
		name = "ada",
		label = "Ada",
		base = "dark",
		palette = { accent = "#5fafff" },
		syntax = [[
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>name</key><string>Ada</string>
  <key>settings</key>
  <array>
    <dict><key>settings</key><dict><key>foreground</key><string>#D0D0D0</string></dict></dict>
    <dict>
      <key>scope</key><string>codex.accent</string>
      <key>settings</key><dict><key>foreground</key><string>#5FAFFF</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>comment, punctuation.definition.comment</string>
      <key>settings</key><dict><key>foreground</key><string>#808080</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>string, punctuation.definition.string</string>
      <key>settings</key><dict><key>foreground</key><string>#87FF87</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>keyword, storage, markup.heading</string>
      <key>settings</key><dict><key>foreground</key><string>#5FAFFF</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>support.type, support.class, support.variable, entity.name.type</string>
      <key>settings</key><dict><key>foreground</key><string>#5FD7FF</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>constant, constant.numeric</string>
      <key>settings</key><dict><key>foreground</key><string>#AF87FF</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>punctuation, keyword.operator</string>
      <key>settings</key><dict><key>foreground</key><string>#BCBCBC</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>markup.inserted</string>
      <key>settings</key><dict>
        <key>foreground</key><string>#00AF00</string>
        <key>background</key><string>#213A2B</string>
      </dict>
    </dict>
    <dict>
      <key>scope</key><string>markup.deleted</string>
      <key>settings</key><dict>
        <key>foreground</key><string>#FF5F5F</string>
        <key>background</key><string>#4A221D</string>
      </dict>
    </dict>
  </array>
</dict>
</plist>
]],
	},
	{
		name = "babbage",
		label = "Babbage",
		base = "light",
		palette = { accent = "#008700" },
		syntax = [[
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>name</key><string>Babbage</string>
  <key>settings</key>
  <array>
    <dict><key>settings</key><dict><key>foreground</key><string>#444444</string></dict></dict>
    <dict>
      <key>scope</key><string>codex.accent</string>
      <key>settings</key><dict><key>foreground</key><string>#008700</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>comment, punctuation.definition.comment</string>
      <key>settings</key><dict><key>foreground</key><string>#626262</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>string, punctuation.definition.string</string>
      <key>settings</key><dict><key>foreground</key><string>#008700</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>keyword, storage, markup.heading</string>
      <key>settings</key><dict><key>foreground</key><string>#008700</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>support.type, support.class, support.variable, entity.name.type</string>
      <key>settings</key><dict><key>foreground</key><string>#0087AF</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>constant, constant.numeric</string>
      <key>settings</key><dict><key>foreground</key><string>#870087</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>punctuation, keyword.operator</string>
      <key>settings</key><dict><key>foreground</key><string>#585858</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>markup.inserted</string>
      <key>settings</key><dict>
        <key>foreground</key><string>#00AF00</string>
        <key>background</key><string>#DAFBE1</string>
      </dict>
    </dict>
    <dict>
      <key>scope</key><string>markup.deleted</string>
      <key>settings</key><dict>
        <key>foreground</key><string>#FF5F5F</string>
        <key>background</key><string>#FFEBE9</string>
      </dict>
    </dict>
  </array>
</dict>
</plist>
]],
	},
	{
		name = "curie",
		label = "Curie",
		base = "dark",
		palette = { accent = "#d7af00" },
		syntax = [[
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>name</key><string>Curie</string>
  <key>settings</key>
  <array>
    <dict><key>settings</key><dict><key>foreground</key><string>#D0D0D0</string></dict></dict>
    <dict>
      <key>scope</key><string>codex.accent</string>
      <key>settings</key><dict><key>foreground</key><string>#D7AF00</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>comment, punctuation.definition.comment</string>
      <key>settings</key><dict><key>foreground</key><string>#808080</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>string, punctuation.definition.string</string>
      <key>settings</key><dict><key>foreground</key><string>#FFAF5F</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>keyword, storage, markup.heading</string>
      <key>settings</key><dict><key>foreground</key><string>#D7AF00</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>support.type, support.class, support.variable, entity.name.type</string>
      <key>settings</key><dict><key>foreground</key><string>#87AFD7</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>constant, constant.numeric</string>
      <key>settings</key><dict><key>foreground</key><string>#D7AF5F</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>punctuation, keyword.operator</string>
      <key>settings</key><dict><key>foreground</key><string>#BCBCBC</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>markup.inserted</string>
      <key>settings</key><dict>
        <key>foreground</key><string>#00AF00</string>
        <key>background</key><string>#213A2B</string>
      </dict>
    </dict>
    <dict>
      <key>scope</key><string>markup.deleted</string>
      <key>settings</key><dict>
        <key>foreground</key><string>#FF5F5F</string>
        <key>background</key><string>#4A221D</string>
      </dict>
    </dict>
  </array>
</dict>
</plist>
]],
	},
	{
		name = "cushman",
		label = "Cushman",
		base = "light",
		palette = { accent = "#0087ff" },
		syntax = [[
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>name</key><string>Cushman</string>
  <key>settings</key>
  <array>
    <dict><key>settings</key><dict><key>foreground</key><string>#444444</string></dict></dict>
    <dict>
      <key>scope</key><string>codex.accent</string>
      <key>settings</key><dict><key>foreground</key><string>#0087FF</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>comment, punctuation.definition.comment</string>
      <key>settings</key><dict><key>foreground</key><string>#626262</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>string, punctuation.definition.string</string>
      <key>settings</key><dict><key>foreground</key><string>#008700</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>keyword, storage, markup.heading</string>
      <key>settings</key><dict><key>foreground</key><string>#005FAF</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>support.type, support.class, support.variable, entity.name.type</string>
      <key>settings</key><dict><key>foreground</key><string>#0087AF</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>constant, constant.numeric</string>
      <key>settings</key><dict><key>foreground</key><string>#870087</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>punctuation, keyword.operator</string>
      <key>settings</key><dict><key>foreground</key><string>#585858</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>markup.inserted</string>
      <key>settings</key><dict>
        <key>foreground</key><string>#00AF00</string>
        <key>background</key><string>#DAFBE1</string>
      </dict>
    </dict>
    <dict>
      <key>scope</key><string>markup.deleted</string>
      <key>settings</key><dict>
        <key>foreground</key><string>#FF5F5F</string>
        <key>background</key><string>#FFEBE9</string>
      </dict>
    </dict>
  </array>
</dict>
</plist>
]],
	},
	{
		name = "dali",
		label = "Dali",
		base = "dark",
		palette = { accent = "#d7af00" },
		syntax = [[
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>name</key><string>Dali</string>
  <key>settings</key>
  <array>
    <dict><key>settings</key><dict><key>foreground</key><string>#D0D0D0</string></dict></dict>
    <dict>
      <key>scope</key><string>codex.accent</string>
      <key>settings</key><dict><key>foreground</key><string>#D7AF00</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>comment, punctuation.definition.comment</string>
      <key>settings</key><dict><key>foreground</key><string>#808080</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>string, punctuation.definition.string</string>
      <key>settings</key><dict><key>foreground</key><string>#FF87FF</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>keyword, storage, markup.heading</string>
      <key>settings</key><dict><key>foreground</key><string>#D7AF00</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>support.type, support.class, support.variable, entity.name.type</string>
      <key>settings</key><dict><key>foreground</key><string>#5FD7FF</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>constant, constant.numeric</string>
      <key>settings</key><dict><key>foreground</key><string>#FF5FFF</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>punctuation, keyword.operator</string>
      <key>settings</key><dict><key>foreground</key><string>#BCBCBC</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>markup.inserted</string>
      <key>settings</key><dict>
        <key>foreground</key><string>#00AF00</string>
        <key>background</key><string>#00000001</string>
      </dict>
    </dict>
    <dict>
      <key>scope</key><string>markup.deleted</string>
      <key>settings</key><dict>
        <key>foreground</key><string>#FF5F5F</string>
        <key>background</key><string>#00000001</string>
      </dict>
    </dict>
  </array>
</dict>
</plist>
]],
	},
	{
		name = "davinci",
		label = "Davinci",
		base = "light",
		palette = { accent = "#d75f00" },
		syntax = [[
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>name</key><string>Davinci</string>
  <key>settings</key>
  <array>
    <dict><key>settings</key><dict><key>foreground</key><string>#444444</string></dict></dict>
    <dict>
      <key>scope</key><string>codex.accent</string>
      <key>settings</key><dict><key>foreground</key><string>#D75F00</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>comment, punctuation.definition.comment</string>
      <key>settings</key><dict><key>foreground</key><string>#626262</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>string, punctuation.definition.string</string>
      <key>settings</key><dict><key>foreground</key><string>#AF5F00</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>keyword, storage, markup.heading</string>
      <key>settings</key><dict><key>foreground</key><string>#AF5F00</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>support.type, support.class, support.variable, entity.name.type</string>
      <key>settings</key><dict><key>foreground</key><string>#005FAF</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>constant, constant.numeric</string>
      <key>settings</key><dict><key>foreground</key><string>#AF8700</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>punctuation, keyword.operator</string>
      <key>settings</key><dict><key>foreground</key><string>#585858</string></dict>
    </dict>
    <dict>
      <key>scope</key><string>markup.inserted</string>
      <key>settings</key><dict>
        <key>foreground</key><string>#00AF00</string>
        <key>background</key><string>#00000001</string>
      </dict>
    </dict>
    <dict>
      <key>scope</key><string>markup.deleted</string>
      <key>settings</key><dict>
        <key>foreground</key><string>#FF5F5F</string>
        <key>background</key><string>#00000001</string>
      </dict>
    </dict>
  </array>
</dict>
</plist>
]],
	},
}
