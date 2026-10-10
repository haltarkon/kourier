pragma Singleton

import QtQuick
import app.kourier

// The emoji the pickers offer: six quick reactions and a list by category. A static set until
// the Rust `EmojiModel` lands (§8.3): no search, no skin tones, no "recently used".
QtObject {
    id: emoji

    // The quick reactions (the same six, in the same order, as Signal's own clients).
    readonly property list<string> quick: ["❤️", "👍", "👎", "😂", "😮", "😢"]

    // `icon` stands for the category on its tab; `name` is its accessible name and tooltip.
    readonly property list<var> categories: [
        {
            "icon": "😀",
            "name": L10n.i18nc("@title:tab emoji category", "Smileys"),
            "emoji": emoji.split("😀 😃 😄 😁 😆 😅 😂 🤣 🙂 🙃 😉 😊 😇 🥰 😍 🤩 😘 😗 😚 😋 😛 😜 🤪 😝 🤑 🤗 🤭 🤫 🤔 🤐 🤨 😐 😑 😶 😏 😒 🙄 😬 😌 😔 😪 😴 😷 🤒 🤕 🤢 🤮 🥵 🥶 🥴 😵 🤯 🤠 🥳 😎 🤓 🧐 😕 😟 🙁 😮 😯 😲 😳 🥺 😦 😧 😨 😰 😥 😢 😭 😱 😖 😣 😞 😓 😩 😫 🥱 😤 😡 😠 🤬 😈 👿 💀 💩 🤡 👻 👽 🤖")
        },
        {
            "icon": "👋",
            "name": L10n.i18nc("@title:tab emoji category", "People"),
            "emoji": emoji.split("👋 🤚 ✋ 🖖 👌 🤏 ✌️ 🤞 🤟 🤘 🤙 👈 👉 👆 👇 ☝️ 👍 👎 ✊ 👊 🤛 🤜 👏 🙌 👐 🤲 🤝 🙏 ✍️ 💪 🦾 👀 👂 👃 🧠 👶 🧒 👦 👧 🧑 👨 👩 🧓 👴 👵 🙅 🙆 💁 🙋 🙇 🤦 🤷 💃 🕺 🚶 🏃 👫 👪")
        },
        {
            "icon": "❤️",
            "name": L10n.i18nc("@title:tab emoji category", "Symbols"),
            "emoji": emoji.split("❤️ 🧡 💛 💚 💙 💜 🖤 🤍 🤎 💔 ❣️ 💕 💞 💓 💗 💖 💘 💝 💯 💢 💥 💫 💦 💨 💤 ✨ ⭐ 🌟 🔥 ✅ ❌ ❓ ❗ ⚠️ 🚫 💬 🔔 🎵 🎶 ➕ ➖ ♻️ ✔️ 🆗 🆒 🆕 🔴 🟠 🟡 🟢 🔵 🟣 ⚫ ⚪")
        },
        {
            "icon": "🐶",
            "name": L10n.i18nc("@title:tab emoji category", "Animals and Nature"),
            "emoji": emoji.split("🐶 🐱 🐭 🐹 🐰 🦊 🐻 🐼 🐨 🐯 🦁 🐮 🐷 🐸 🐵 🙈 🙉 🙊 🐔 🐧 🐦 🦆 🦅 🦉 🐺 🐴 🦄 🐝 🦋 🐌 🐞 🐢 🐍 🐙 🐬 🐳 🐟 🌵 🎄 🌲 🌳 🌴 🌱 🌿 🍀 🍁 🍂 🌷 🌹 🌻 🌼 🌸 🌍 🌙 ☀️ ⛅ ☁️ 🌧️ ⛈️ ❄️ 🌈 🌊")
        },
        {
            "icon": "🍕",
            "name": L10n.i18nc("@title:tab emoji category", "Food and Drink"),
            "emoji": emoji.split("🍏 🍎 🍐 🍊 🍋 🍌 🍉 🍇 🍓 🍒 🍑 🥭 🍍 🥝 🍅 🥑 🥦 🥕 🌽 🥔 🍞 🥐 🧀 🥚 🍳 🥓 🍔 🍟 🍕 🌭 🥪 🌮 🌯 🥗 🍝 🍜 🍲 🍣 🍱 🍤 🍙 🍦 🍰 🎂 🍪 🍫 🍬 🍿 ☕ 🍵 🥤 🍺 🍻 🥂 🍷 🥃 🍸 🍹 🍾")
        },
        {
            "icon": "⚽",
            "name": L10n.i18nc("@title:tab emoji category", "Activities"),
            "emoji": emoji.split("⚽ 🏀 🏈 ⚾ 🎾 🏐 🏉 🎱 🏓 🏸 🥅 ⛳ 🏹 🎣 🥊 🛹 ⛷️ 🏂 🏋️ 🚴 🏊 🧗 🏆 🥇 🥈 🥉 🏅 🎖️ 🎫 🎭 🎨 🎬 🎤 🎧 🎼 🎹 🥁 🎷 🎺 🎸 🎻 🎲 ♟️ 🎯 🎳 🎮 🧩 🎉 🎊 🎈 🎁")
        },
        {
            "icon": "🚗",
            "name": L10n.i18nc("@title:tab emoji category", "Travel and Places"),
            "emoji": emoji.split("🚗 🚕 🚌 🚎 🏎️ 🚓 🚑 🚒 🚚 🚜 🛵 🏍️ 🚲 🛴 🚂 🚆 🚇 🚊 ✈️ 🛫 🛬 🚀 🛸 🚁 ⛵ 🚤 🚢 ⚓ ⛽ 🚦 🗺️ 🗿 🗽 🗼 🏰 🏯 🏟️ 🎡 🎢 ⛲ ⛱️ 🏖️ 🏝️ 🏜️ 🌋 ⛰️ 🏔️ 🏕️ ⛺ 🏠 🏡 🏢 🏥 🏦 🏨 🏪 🏫 ⛪ 🌃 🌆 🌉")
        },
        {
            "icon": "💡",
            "name": L10n.i18nc("@title:tab emoji category", "Objects"),
            "emoji": emoji.split("⌚ 📱 💻 ⌨️ 🖥️ 🖨️ 🖱️ 💾 💿 📷 📹 🎥 📞 ☎️ 📺 📻 🧭 ⏱️ ⏰ ⌛ 🔋 🔌 💡 🔦 🕯️ 🧯 🛠️ 🔧 🔨 ⚙️ 🔩 🧲 💣 🔪 🛡️ 🔑 🗝️ 🔒 🔓 📦 ✉️ 📧 📝 📁 📂 📅 📌 📎 ✂️ 📏 📚 📖 🔖 💰 💳 🧾 🛒 🎒 👓 🕶️ 👕 👟 🎩 👑 💍")
        }
    ]

    function split(list: string): list<string> {
        return list.split(" ");
    }
}
