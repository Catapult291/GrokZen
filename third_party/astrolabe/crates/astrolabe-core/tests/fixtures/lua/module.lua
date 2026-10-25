-- Lua extraction fixture (lang-batch-8). Line numbers are asserted in parse/lua.rs.

local M = {}
local a, b = 1, 2
local lazy
VERSION = "1.0"
M.name = "fixture"
M.handler = function(x) return x end

function M.foo(x) return x end

function M:bar() return self end

local function baz() end

function glob() end

local cfg = {
  timeout = 30,
  on_attach = function(client) end,
  nested = { inner_attach = function() end, deep = 1 },
  [1] = "positional",
}

local function outer()
  local helper = function() end
  M.cb = function() end
  local plain = 1
end

local function deps()
  local strings = require("util.strings")
  local tables = require 'util.tables'
  local ab = require"a.b"
  local long = require [[lib.long]]
  dofile("conf.lua")
  local x = other("nope")
end

return {
  filetypes = { "lua" },
  cmd = "lua-language-server",
  setup = function() end,
}
