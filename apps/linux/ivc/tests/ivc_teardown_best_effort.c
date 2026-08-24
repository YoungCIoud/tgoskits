#include <ivc/ioctl_args.h>
#include <ivc/ulib.h>

#include <errno.h>
#include <stdarg.h>
#include <stdio.h>

static int close_requests;
static int cleanup_requests;
static int free_requests;
static int expected_channel_fd;
static int expected_manager_fd;
static unsigned long expected_cleanup_request;
static void *expected_cleanup_arg;
static void *expected_free_arg;
static int unexpected_call;
static int close_result;
static int cleanup_result;

static void reset_mocks(int channel_fd,
                        int manager_fd,
                        unsigned long cleanup_request,
                        void *cleanup_arg,
                        void *free_arg)
{
    close_requests = 0;
    cleanup_requests = 0;
    free_requests = 0;
    expected_channel_fd = channel_fd;
    expected_manager_fd = manager_fd;
    expected_cleanup_request = cleanup_request;
    expected_cleanup_arg = cleanup_arg;
    expected_free_arg = free_arg;
    unexpected_call = 0;
    close_result = 0;
    cleanup_result = 0;
}

int __wrap_close(int fd)
{
    close_requests++;
    if (fd != expected_channel_fd) {
        unexpected_call = 1;
    }
    if (close_result < 0) {
        errno = EIO;
    }
    return close_result;
}

int __wrap_ioctl(int fd, unsigned long request, ...)
{
    va_list args;
    void *arg;

    va_start(args, request);
    arg = va_arg(args, void *);
    va_end(args);

    cleanup_requests++;
    if (fd != expected_manager_fd || request != expected_cleanup_request ||
        arg != expected_cleanup_arg) {
        unexpected_call = 1;
    }
    if (cleanup_result < 0) {
        errno = EIO;
    }
    return cleanup_result;
}

void __wrap_free(void *ptr)
{
    free_requests++;
    if (ptr != expected_free_arg) {
        unexpected_call = 1;
    }
}

static int expect_failed_teardown_completed(const char *operation, int result)
{
    if (result != -1) {
        fprintf(stderr,
                "%s: expected teardown failure to be reported, got %d\n",
                operation,
                result);
        return 1;
    }
    if (close_requests != 1) {
        fprintf(stderr, "%s: expected one close call, got %d\n", operation, close_requests);
        return 1;
    }
    if (cleanup_requests != 1) {
        fprintf(stderr, "%s: expected one cleanup ioctl, got %d\n", operation, cleanup_requests);
        return 1;
    }
    if (free_requests != 1) {
        fprintf(stderr, "%s: expected one free call, got %d\n", operation, free_requests);
        return 1;
    }
    if (unexpected_call != 0) {
        fprintf(stderr,
                "%s: cleanup used an unexpected descriptor, request, argument, or allocation\n",
                operation);
        return 1;
    }
    return 0;
}

static int run_unpublish_failure(const char *operation,
                                 int mocked_close_result,
                                 int mocked_cleanup_result)
{
    ivc_manager_t manager = {
        .fd = 7,
    };
    ivc_publisher_t publisher = {
        .manager = &manager,
        .publish_arg = {
            .channel_key = 0x100,
        },
        .fd = 42,
    };

    reset_mocks(publisher.fd,
                manager.fd,
                IVC_UNPUBLISH_CHANNEL,
                &publisher.publish_arg,
                &publisher);
    close_result = mocked_close_result;
    cleanup_result = mocked_cleanup_result;

    return expect_failed_teardown_completed(operation, ivc_unpublish(&publisher));
}

static int run_unsubscribe_failure(const char *operation,
                                   int mocked_close_result,
                                   int mocked_cleanup_result)
{
    ivc_manager_t manager = {
        .fd = 7,
    };
    ivc_subscriber_t subscriber = {
        .manager = &manager,
        .subscribe_arg = {
            .target_publisher_id = 1,
            .channel_key = 0x100,
        },
        .fd = 42,
    };

    reset_mocks(subscriber.fd,
                manager.fd,
                IVC_UNSUBSCRIBE_CHANNEL,
                &subscriber.subscribe_arg,
                &subscriber);
    close_result = mocked_close_result;
    cleanup_result = mocked_cleanup_result;

    return expect_failed_teardown_completed(operation, ivc_unsubscribe(&subscriber));
}

int main(void)
{
    if (run_unpublish_failure("unpublish after close failure", -1, 0) != 0 ||
        run_unpublish_failure("unpublish after ioctl failure", 0, -1) != 0 ||
        run_unsubscribe_failure("unsubscribe after close failure", -1, 0) != 0 ||
        run_unsubscribe_failure("unsubscribe after ioctl failure", 0, -1) != 0) {
        return 1;
    }

    return 0;
}
