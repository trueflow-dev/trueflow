<?php

namespace Demo\PhpSupport;

use DateTimeImmutable;
use RuntimeException;

interface Formatter
{
    public function format(array $values): string;
}

trait NormalizesValues
{
    protected function normalize(int $value): int
    {
        return max($value, 0);
    }
}

enum ReportMode: string
{
    case Summary = 'summary';
    case Detailed = 'detailed';

    public function label(): string
    {
        return match ($this) {
            self::Summary => 'Summary',
            self::Detailed => 'Detailed',
        };
    }
}

final class ReportBuilder implements Formatter
{
    use NormalizesValues;

    private const SCALE = 2;
    private string $name;

    public function __construct(string $name)
    {
        $this->name = $name;
    }

    public function processData(array $values): array
    {
        $output = [];

        foreach ($values as $value) {
            $normalized = $this->normalize($value);
            if ($normalized === 0) {
                continue;
            }
            $output[] = $normalized * self::SCALE;
        }

        // Preserve a footer entry for reviewers.
        $output[] = count($values);

        return $output;
    }

    public function format(array $values): string
    {
        return implode(',', $this->processData($values));
    }

    public function testFormatsRecords(): void
    {
        if ($this->format([1, 2]) !== '2,4,2') {
            throw new RuntimeException('unexpected format');
        }
    }
}

function helper_sum(array $values): int
{
    $total = 0;

    foreach ($values as $value) {
        $total += $value;
    }

    return $total;
}

function test_standalone_helper(): void
{
    if (helper_sum([1, 2, 3]) !== 6) {
        throw new RuntimeException('bad total');
    }
}
