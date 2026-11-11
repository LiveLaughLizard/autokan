// Autokan plugin for opencode (v2): gives opencode the Autokan board tools (list_boards,
// get_board, create_card, move_card, add_comment, ...) by connecting it to Autokan's MCP server.
//
// Install: copy this file to `.opencode/plugins/autokan.ts` in a project, or to
// `~/.config/opencode/plugins/autokan.ts` for every project, and set
//
//   AUTOKAN_URL       e.g. https://autokan.example.com
//   AUTOKAN_API_KEY   a key from Autokan's "API keys" page (user menu)
//
// in the environment opencode's server runs in (`opencode service restart` after changing them).
// The tools run as the key's owner, with that user's permissions.
// Checked against opencode 2.2.1.
import { Plugin } from "@opencode/plugin"

export default Plugin.define({
  id: "autokan",
  async setup(ctx) {
    const url = (process.env.AUTOKAN_URL || "").replace(/\/+$/, "")
    const key = process.env.AUTOKAN_API_KEY || ""
    if (!url || !key) {
      console.warn("autokan: set AUTOKAN_URL and AUTOKAN_API_KEY to enable the Autokan tools")
      return
    }
    await ctx.mcp.transform((editor) => {
      editor.set("autokan", {
        type: "remote",
        url: `${url}/mcp`,
        oauth: false,
        headers: { Authorization: `Bearer ${key}` },
      })
    })
  },
})
