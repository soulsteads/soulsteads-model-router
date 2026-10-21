pub(super) fn embedding_input_context_tokens(input: &str) -> u64 {
    // The embedding model's tokenizer contract is not published/configured here.
    // This remains a byte-length heuristic, not a validated token upper bound.
    // Reserve 32 estimated tokens for framing until a model-specific contract exists.
    (input.len() as u64).saturating_add(32)
}

pub(super) fn embedding_batch_context_tokens(inputs: &[&str]) -> u64 {
    // Embedding batches are independent sequences, so fit is the largest item,
    // not the sum of token counts across the batch.
    inputs
        .iter()
        .map(|input| embedding_input_context_tokens(input))
        .max()
        .unwrap_or(0)
}
