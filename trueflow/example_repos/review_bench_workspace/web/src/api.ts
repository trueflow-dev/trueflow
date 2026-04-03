export type ReviewStatus = "unreviewed" | "approved" | "commented";

export interface ReviewRecord {
  path: string;
  status: ReviewStatus;
  note?: string;
}

export interface RequestOptions {
  retries: number;
  signal?: AbortSignal;
}

async function parseJson<T>(response: Response): Promise<T> {
  if (!response.ok) {
    throw new Error(`request failed: ${response.status}`);
  }

  return (await response.json()) as T;
}

export async function fetchReviewSummary(options: RequestOptions): Promise<ReviewRecord[]> {
  const response = await fetch("/api/review/summary", {
    headers: { "accept": "application/json" },
    signal: options.signal,
  });
  return parseJson<ReviewRecord[]>(response);
}

export async function retryingFetchReviewSummary(options: RequestOptions): Promise<ReviewRecord[]> {
  let attempts = 0;
  let lastError: Error | undefined;

  while (attempts <= options.retries) {
    try {
      return await fetchReviewSummary(options);
    } catch (error) {
      lastError = error as Error;
      attempts += 1;
    }
  }

  throw lastError ?? new Error("request failed without an error");
}
