package demo

import (
    "fmt"
    "strings"
)

const maxRetries = 3
var defaultPrefix = "worker"

type Worker struct {
    id int
    retries int
    factor int
}

type Runner interface {
    Run(input string) string
    Reset()
}

func (w Worker) Process(values []int) []int {
    output := make([]int, 0, len(values))

    // scale values before returning them
    for _, value := range values {
        if value > w.factor {
            output = append(output, value*w.factor)
        } else {
            output = append(output, value+w.factor)
        }
    }

    return output
}

func collectUntil(limit int) []int {
    values := make([]int, 0, limit)
    for current := 0; current < limit; current++ {
        values = append(values, current)
    }
    return values
}

func main() {
    processor := Worker{id: 7, retries: maxRetries, factor: 2}
    values := collectUntil(4)
    fmt.Println(strings.ToUpper(defaultPrefix), processor.Process(values))
}
