return {
  { "akinsho/bufferline.nvim", enabled = false },
  {
    "nvim-lualine/lualine.nvim",
    opts = function()
      local box = "#313244"
      return {
        options = {
          theme = "catppuccin",
          globalstatus = true,
          component_separators = "",
          section_separators = "",
        },
        sections = {
          lualine_a = { "mode" },
          lualine_b = { { "branch", color = { bg = box } }, { "diagnostics", color = { bg = box } } },
          lualine_c = { { "filename", color = { bg = box } } },
          lualine_x = { { "filetype", color = { bg = box } } },
          lualine_y = { { "progress", color = { bg = box } } },
          lualine_z = {
            { "location", color = { bg = box } },
            { function() return os.date("%H:%M") end, color = { bg = box } },
          },
        },
      }
    end,
  },
  {
    "folke/noice.nvim",
    opts = {
      views = {
        popup = { border = "single" },
        cmdline_popup = { border = { style = "single", padding = { 0, 1 } } },
        cmdline_input = { border = { style = "single", padding = { 0, 1 } } },
        confirm = { border = { style = "single", padding = { 0, 1 }, text = { top = " Confirm " } } },
      },
    },
  },
  { "folke/flash.nvim", enabled = false },
  {
    "LazyVim/LazyVim",
    opts = {
      colorscheme = "catppuccin",
    },
  },
  {
    "catppuccin/nvim",
    name = "catppuccin",
    priority = 1000,
    opts = function()
      return {
        flavour = "mocha",
        background = { light = "mocha", dark = "mocha" },
        transparent_background = false,
        show_end_of_buffer = false,
        term_colors = true,
        dim_inactive = { enabled = false, shade = "dark", percentage = 0.15 },
        no_italic = false,
        no_bold = false,
        no_underline = false,
        styles = {
          comments = { "italic" },
          conditionals = { "italic" },
          loops = {},
          functions = {},
          keywords = {},
          strings = {},
          variables = {},
          numbers = {},
          booleans = {},
          properties = {},
          types = {},
          operators = {},
        },
        integrations = {
          cmp = true,
          gitsigns = true,
          nvimtree = true,
          telescope = { enabled = true },
          treesitter = true,
          notify = false,
          mini = false,
          mason = true,
          which_key = true,
        },
      }
    end,
  },
  {
    "snacks.nvim",
    opts = {
      dashboard = { enabled = false },
      image = { enabled = false },
      lazygit = { enabled = false },
    },
  },
}