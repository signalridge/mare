const response = {
  responseId: 'resp-tool',
  candidates: [{
    content: {
      role: 'model',
      parts: [{
        functionCall: { id: 'native-call', name: 'weather', args: { city: 'sf' } },
        thoughtSignature: 'sig-tool',
      }],
    },
    finishReason: 'STOP',
  }],
};
