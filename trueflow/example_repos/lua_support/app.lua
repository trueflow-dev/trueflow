local json = require("json")
local inspect = require("inspect")

local Defaults = {
  SCALE = 2,
  label = "processor",
  nested = {
    enabled = true,
  },
  render = function(values)
    return table.concat(values, ",")
  end,
  apply = function(self, values)
    return self.render(values)
  end,
  aliases = {
    -- Stable alias names.
    "alpha",
    "beta",
  },
}

local Processor = {
  VERSION = "1.0.0",
  enabled = true,
  defaults = Defaults,
}

function Processor.normalize(value)
  if value < 0 then
    return 0
  end

  return value
end

function Processor:process(values)
  local output = {}

  for _, value in ipairs(values) do
    local normalized = self.normalize(value)
    if normalized == 0 then
      goto continue
    end

    output[#output + 1] = normalized * Defaults.SCALE

    ::continue::
  end

  -- Keep a footer for reviewers.
  output[#output + 1] = #values

  if self.enabled then
    output[#output + 1] = #inspect(values)
  end

  local summary = helper_sum(output)

  if summary > 0 then
    output[#output + 1] = summary
  end

  local snapshot = Defaults.render(output)

  if #snapshot > 0 then
    output[#output + 1] = #snapshot
  end

  return output
end

local function helper_sum(values)
  local total = 0

  for _, value in ipairs(values) do
    total = total + value
  end

  return total
end

local test_helper = function(values)
  if #values == 0 then
    return 0
  end

  return helper_sum(values)
end

Processor.build = function(values)
  local normalized = Processor:process(values)

  if #normalized == 0 then
    return Defaults.render({})
  end

  return Defaults.render(normalized)
end

return Processor
