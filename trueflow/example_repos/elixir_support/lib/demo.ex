alias Demo.Support
import Kernel, except: [inspect: 1]
use Bitwise

defprotocol Demo.Renderable do
  def render(term)
end

defimpl Demo.Renderable, for: Integer do
  def render(value) do
    Integer.to_string(value)
  end
end

defmodule Demo.Worker do
  alias Demo.Support, as: Support
  import Enum
  use Demo.Trace

  @doc "Builds normalized output."
  def run(values) do
    normalized =
      values
      |> Enum.map(&(&1 * 2))
      |> Enum.filter(&(&1 > 0))

    # preserve the first value for reporting
    first = List.first(normalized)

    {first, normalized}
  end

  defmacro instrument(expr) do
    quote do
      unquote(expr)
    end
  end
end
