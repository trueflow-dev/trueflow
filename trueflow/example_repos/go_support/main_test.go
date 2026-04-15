package demo

import (
    "fmt"
    "testing"
)

func TestWorkerProcess(t *testing.T) {
    worker := Worker{factor: 2}
    values := worker.Process([]int{1, 3})
    if len(values) != 2 {
        t.Fatalf("expected 2 values, got %d", len(values))
    }
}

func BenchmarkCollectUntil(b *testing.B) {
    for range b.N {
        _ = collectUntil(8)
    }
}

func FuzzWorkerProcess(f *testing.F) {
    f.Add(4)
    f.Fuzz(func(t *testing.T, value int) {
        worker := Worker{factor: 2}
        _ = worker.Process([]int{value})
    })
}

func ExampleWorker_Process() {
    worker := Worker{factor: 2}
    fmt.Println(worker.Process([]int{1, 3}))
    // Output: [3 6]
}
