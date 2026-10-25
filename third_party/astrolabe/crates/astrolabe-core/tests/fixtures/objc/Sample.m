#import <Foundation/Foundation.h>
#import "AFURLSessionManager.h"
@import UIKit;

NS_ASSUME_NONNULL_BEGIN

#define AF_MAX 16
#define AF_MIN(a, b) ((a) < (b) ? (a) : (b))

typedef NS_ENUM(NSInteger, AFStatus) {
    AFStatusA = 1,
    AFStatusB,
};

typedef NS_OPTIONS(NSUInteger, AFOptions) {
    AFOptionA = 1 << 0,
    AFOptionB = 1 << 1,
};

typedef NS_ERROR_ENUM(AFErrorDomain, AFError) {
    AFErrorTimeout = -1,
};

typedef void (^AFBlock)(id obj);
static NSString * const kKey = @"k";
NSString * const AFExt = @"x";
int gCounter = 0;

struct S { int a; };

@class Foo, Bar;
@protocol Fwd;

@protocol AFProto <NSObject>
@optional
- (void)foo:(int)a bar:(int)b;
@property (nonatomic) int count;
@end

@interface AFMgr : NSObject <AFProto> {
    int _ivar;
    char *_buffer;
}
@property (readonly, nonatomic, strong) NSURLSession *session;
@property (nonatomic, copy, nullable) void (^completion)(BOOL ok);
+ (instancetype)manager;
- (instancetype)initWithX:(NSString *)x NS_DESIGNATED_INITIALIZER;
- (void)availableMethod API_AVAILABLE(ios(13.0), macos(10.15));
@end

@interface AFMgr (Cat)
- (void)catMethod;
@end

NS_ASSUME_NONNULL_END

@implementation AFMgr
RCT_EXPORT_MODULE()

RCT_EXPORT_METHOD(exported:(NSString *)name resolver:(id)resolve)
{
}

RCT_REMAP_METHOD(jsName, remapped:(int)x)
{
}

+ (instancetype)manager { return [[self alloc] init]; }
- (void)foo:(int)a bar:(int)b {
    int local = 0;
    [self foo:local bar:b];
#if TARGET_OS_IOS
    if (a) {
#else
    if (b) {
#endif
        NSLog(@"x");
    }
}
- (void)afterIf {}
- (instancetype)initWithX:(NSString *)x { return self; }
- (void)availableMethod {}
@end

static void helper(int x) { helper(1); }
void AFPublic(void) {}
