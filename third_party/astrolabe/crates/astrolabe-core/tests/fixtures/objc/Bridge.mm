#import "RCTBridgeThing.h"
#include <memory>

namespace facebook::react {

static constexpr int kLimit = 4;

enum class Mode { Fast, Slow };

class Helper {
 public:
  int compute(int x) const;
};

int Helper::compute(int x) const { return x * 2; }

void freeFunction(std::shared_ptr<Helper> h) {
  auto f = [h](int v) { return h->compute(v); };
  f(1);
}

} // namespace facebook::react

@implementation RCTBridgeThing {
  std::unique_ptr<facebook::react::Helper> _helper;
}

- (instancetype)initWithRuntime:(std::shared_ptr<int>)runtime
{
  if (self = [super init]) {
    _helper = std::make_unique<facebook::react::Helper>();
  }
  return self;
}

- (void)invoke
{
  facebook::react::freeFunction(nullptr);
}

@end
