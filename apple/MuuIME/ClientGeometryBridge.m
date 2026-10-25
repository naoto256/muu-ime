#import "ClientGeometryBridge.h"
#import <InputMethodKit/InputMethodKit.h>
#include <math.h>
#include <string.h>

// Some clients raise an NSException from this query. Catching it here keeps
// the exception from unwinding through Swift frames, which is undefined.
MuuClientLineGeometry MuuClientReadLineGeometry(id client, NSUInteger index) {
    MuuClientLineGeometry result = {0};
    result.lineRect = NSMakeRect(NAN, NAN, NAN, NAN);
    result.orientation = -1;
    result.lineHeight = NAN;
    @try {
        NSDictionary *attributes = [client attributesForCharacterIndex:index
                                                 lineHeightRectangle:&result.lineRect];
        result.returnedAttributes = attributes != nil;
        id baseline = attributes[@"IMKBaseline"];
        if ([baseline isKindOfClass:[NSValue class]] &&
            strcmp([baseline objCType], @encode(NSPoint)) == 0) {
            result.hasBaseline = YES;
            result.baseline = [baseline pointValue];
        }
        id orientation = attributes[IMKTextOrientationName];
        // Only 0 and 1 are meaningful (a BOOL: 1 is horizontal, 0 vertical,
        // the reverse of NSTextLayoutOrientation). Any other value stays -1,
        // "not reported".
        if ([orientation isKindOfClass:[NSNumber class]]) {
            double value = [orientation doubleValue];
            if (value == 0 || value == 1) result.orientation = (NSInteger)value;
        }
        id lineHeight = attributes[@"IMKLineHeight"];
        if ([lineHeight isKindOfClass:[NSNumber class]]) result.lineHeight = [lineHeight doubleValue];
    } @catch (NSException *exception) {
        // Do not copy the exception's reason, attributes, font or text. A
        // caller decides whether to hide or retain its checked fallback.
        result.exception = YES;
    }
    return result;
}

#if defined(MUU_TEST)
// Test clients whose attributes query throws, to check that the exception
// stops here and its reason text is never copied.
@interface MuuThrowingAttributeClient : NSObject
@end
@implementation MuuThrowingAttributeClient
- (NSDictionary *)attributesForCharacterIndex:(NSUInteger)index lineHeightRectangle:(NSRect *)rect {
    @throw [NSException exceptionWithName:@"MuuFixtureException" reason:@"CANARY_EXCEPTION_BODY" userInfo:nil];
}
@end
id MuuInputPathThrowingClientForTesting(void) { return [[MuuThrowingAttributeClient alloc] init]; }

// Real IMKTextInput dispatch with only attributes implemented in Objective-C:
// the exception is thrown and caught without unwinding through a Swift fake.
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wprotocol"
@interface MuuForwardingThrowingAttributeClient : MuuThrowingAttributeClient <IMKTextInput>
@property(nonatomic, strong) id target;
@property(nonatomic) BOOL throwsOnlyAtIndex;
@property(nonatomic) NSUInteger throwingIndex;
@end
@implementation MuuForwardingThrowingAttributeClient
- (id)forwardingTargetForSelector:(SEL)selector { return self.target; }
- (NSDictionary *)attributesForCharacterIndex:(NSUInteger)index lineHeightRectangle:(NSRect *)rect {
    if (!self.throwsOnlyAtIndex || index == self.throwingIndex) {
        return [super attributesForCharacterIndex:index lineHeightRectangle:rect];
    }
    return [self.target attributesForCharacterIndex:index lineHeightRectangle:rect];
}
@end
#pragma clang diagnostic pop
id MuuInputPathThrowingClientWithTargetForTesting(id target) {
    MuuForwardingThrowingAttributeClient *client = [[MuuForwardingThrowingAttributeClient alloc] init];
    client.target = target;
    return client;
}
id MuuInputPathThrowingClientAtIndexForTesting(id target, NSUInteger index) {
    MuuForwardingThrowingAttributeClient *client = MuuInputPathThrowingClientWithTargetForTesting(target);
    client.throwsOnlyAtIndex = YES;
    client.throwingIndex = index;
    return client;
}
#endif
