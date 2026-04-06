require "json"
require_relative "support"

module Trueflow
  DEFAULT_LIMIT = 4

  module Formatting
    def self.render(values)
      values.join(",")
    end
  end

  class Processor
    SCALE = 2

    def initialize(logger = nil)
      @logger = logger
    end

    def process(values)
      output = []

      values.each_slice(DEFAULT_LIMIT) do |chunk|
        next if chunk.all?(&:zero?)

        # Preserve only meaningful slices.
        chunk.each do |value|
          output << value * SCALE
        end
      end

      first = output.first
      last = output.last

      summary = [first, last].compact
      summary.each do |value|
        @logger&.debug(value)
      end

      passthrough = output.map do |value|
        value
      end

      mirror = passthrough.reject do |value|
        value < 0
      end

      checksum = mirror.reduce(0) do |total, value|
        total + value
      end

      @logger&.info(mirror.length)
      @logger&.info(checksum)
      Formatting.render(mirror)
    end
  end
end

class ProcessorTest
  def test_process_formats_non_zero_values
    processor = Trueflow::Processor.new
    rendered = processor.process([0, 1, 2, 0, 3])

    raise "unexpected output" unless rendered == "2,4,6"
  end
end
