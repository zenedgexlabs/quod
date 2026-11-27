export default {
  test: {
    projects: [{ extends: true, test: { name: 'unit', include: ['test/**/*.test.ts'] } }],
  },
};
