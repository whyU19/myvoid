vim.g.loaded_node_provider = 0
vim.g.loaded_perl_provider = 0
vim.g.loaded_python3_provider = 0
vim.g.loaded_ruby_provider = 0

vim.opt.winborder = "single"

-- System clipboard through the `clipboard` tool (Wayland, no wl-clipboard needed)
local cb = (os.getenv("HOME") or "") .. "/.config/clipboard/clipboard"
vim.g.clipboard = {
  name = "clipboard",
  copy = { ["+"] = { cb }, ["*"] = { cb } },
  paste = { ["+"] = { cb, "paste" }, ["*"] = { cb, "paste" } },
  cache_enabled = 0,
}
vim.opt.clipboard = "unnamedplus"
