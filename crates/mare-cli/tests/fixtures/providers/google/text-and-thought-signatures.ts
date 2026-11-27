const usageMetadata = {
  promptTokenCount: 10,
  cachedContentTokenCount: 2,
  candidatesTokenCount: 3,
};

const response = {
  responseId: 'resp-google',
  candidates: [{
    content: {
      role: 'model',
      parts: [
        { text: 'answer', thoughtSignature: 'sig-text' },
        { text: 'reason', thought: true, thoughtSignature: 'sig-think' },
      ],
    },
    finishReason: 'STOP',
  }],
  usageMetadata,
};
