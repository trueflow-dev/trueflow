package main

import "fmt"

const maxRetries = 3

type Multiplier struct {
	factor int
}

func (m Multiplier) Process(values []int) []int {
	output := make([]int, 0, len(values))
	for _, value := range values {
		output = append(output, value*m.factor)
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
	processor := Multiplier{factor: 2}
	values := collectUntil(4)
	processed := processor.Process(values)
	for attempt := 0; attempt < maxRetries; attempt++ {
		fmt.Printf("attempt %d\n", attempt)
	}
	fmt.Println(processed)
}
