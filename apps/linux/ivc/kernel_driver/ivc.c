#include <linux/delay.h>
#include <linux/fs.h>
#include <linux/io.h>
#include <linux/jiffies.h>
#include <linux/list.h>
#include <linux/miscdevice.h>
#include <linux/mutex.h>
#include <linux/slab.h>
#include <linux/uaccess.h>

#include "includes/hvc.h"
#include "includes/ivc.h"
#include "includes/message.h"
#include "includes/region.h"
#include "includes/utils.h"

#define AXIVC_TRANSFER_TIMEOUT_MS 30000
#define AXIVC_SUBSCRIBE_WAIT_MS 10000
#define AXIVC_POLL_INTERVAL_MS 10

struct axivc_endpoint
{
	struct axivc_message_sender tx;
	struct axivc_message_receiver rx;
	struct mutex tx_lock;
	struct mutex rx_lock;
	int tx_error;
};

struct axivc_hvc_output
{
	u64 shm_base;
	u64 shm_size;
};

struct axivc_publisher_vdev
{
	struct miscdevice misc;
	char name[64];
	int id;
	bool active;
	bool removing;
	u64 key;
	u64 shm_base;
	u64 shm_size;
	void *mapped_shm_base;
	struct axivc_endpoint endpoint;
	struct list_head list;
};

struct axivc_subscriber_vdev
{
	struct miscdevice misc;
	char name[64];
	int id;
	bool active;
	bool removing;
	u64 publisher_id;
	u64 key;
	u64 shm_base;
	u64 shm_size;
	void *mapped_shm_base;
	struct axivc_endpoint endpoint;
	struct list_head list;
};

static int pub_vdev_count;
static DEFINE_MUTEX(pub_vdev_lock);
static LIST_HEAD(pub_vdev_list_head);
static int next_pub_vdev_id;

static int sub_vdev_count;
static DEFINE_MUTEX(sub_vdev_lock);
static LIST_HEAD(sub_vdev_list_head);
static int next_sub_vdev_id;

static const struct file_operations axivc_publisher_fops;
static const struct file_operations axivc_subscriber_fops;

static ssize_t axivc_send_message(
	struct axivc_endpoint *endpoint, bool notify_publisher, u64 publisher_id,
	u64 key, const char __user *buf, size_t count);
static ssize_t axivc_recv_message(
	struct axivc_endpoint *endpoint, bool notify_publisher, u64 publisher_id,
	u64 key, char __user *buf, size_t count);
static void axivc_abort_send(
	struct axivc_endpoint *endpoint, bool notify_publisher, u64 publisher_id,
	u64 key);

static void axivc_endpoint_init(
	struct axivc_endpoint *endpoint, struct axivc_ring *tx_ring,
	struct axivc_ring *rx_ring)
{
	axivc_message_sender_init(&endpoint->tx, tx_ring);
	axivc_message_receiver_init(&endpoint->rx, rx_ring);
	mutex_init(&endpoint->tx_lock);
	mutex_init(&endpoint->rx_lock);
	endpoint->tx_error = 0;
}

static void axivc_rollback_publish(u64 key)
{
	if (hvc_unpublish_channel(key))
		WARNING("axvisor: Failed to roll back channel 0x%llx\n", key);
}

static void axivc_rollback_subscribe(u64 publisher_id, u64 key)
{
	if (hvc_unsubscribe_channel(publisher_id, key))
		WARNING(
			"axvisor: Failed to roll back subscription %llu/0x%llx\n",
			publisher_id, key);
}

static int
ivc_publish_channel(u64 channel_key, u64 expected_shm_size, char *pub_dev_name)
{
	u64 shm_base_ptr;
	u64 shm_size_ptr;
	struct axivc_publisher_vdev *vdev;
	struct axivc_hvc_output *hvc_output;
	struct axivc_region *region;
	void *mapped_shm_base;
	u64 shm_base;
	u64 shm_size;
	int id;
	int ret;

	hvc_output = kzalloc(sizeof(*hvc_output), GFP_KERNEL);
	if (!hvc_output)
		return -ENOMEM;
	hvc_output->shm_size = expected_shm_size;
	shm_base_ptr = kva2pa((u64)&hvc_output->shm_base);
	shm_size_ptr = kva2pa((u64)&hvc_output->shm_size);
	if (shm_base_ptr == ~0ULL || shm_size_ptr == ~0ULL)
	{
		kfree(hvc_output);
		return -EFAULT;
	}

	ret = hvc_publish_channel(channel_key, shm_base_ptr, shm_size_ptr);
	if (ret)
	{
		kfree(hvc_output);
		return -EIO;
	}
	shm_base = hvc_output->shm_base;
	shm_size = hvc_output->shm_size;
	kfree(hvc_output);

	/* The IVC rings are cache-coherent shared RAM, not MMIO. Message V1
	 * synchronization uses normal-memory acquire/release operations, so map
	 * the guest physical range with WB semantics rather than ioremap's
	 * device-memory semantics. */
	mapped_shm_base = memremap(shm_base, shm_size, MEMREMAP_WB);
	if (!mapped_shm_base)
	{
		ret = -ENOMEM;
		goto rollback_channel;
	}

	ret = axivc_region_init_publisher(mapped_shm_base, shm_size, channel_key);
	if (ret)
		goto unmap_region;

	vdev = kzalloc(sizeof(*vdev), GFP_KERNEL);
	if (!vdev)
	{
		ret = -ENOMEM;
		goto unmap_region;
	}

	mutex_lock(&pub_vdev_lock);
	if (pub_vdev_count >= MAX_VDEVS)
	{
		mutex_unlock(&pub_vdev_lock);
		kfree(vdev);
		ret = -ENOSPC;
		goto unmap_region;
	}
	id = next_pub_vdev_id++;
	pub_vdev_count++;
	mutex_unlock(&pub_vdev_lock);
	snprintf(
		vdev->name, sizeof(vdev->name), "%s%d", IVC_PUBLISHER_DEV_NAME_PREFIX,
		id);
	vdev->misc.minor = MISC_DYNAMIC_MINOR;
	vdev->misc.name = vdev->name;
	vdev->misc.fops = &axivc_publisher_fops;
	vdev->id = id;
	vdev->key = channel_key;
	vdev->shm_base = shm_base;
	vdev->shm_size = shm_size;
	vdev->mapped_shm_base = mapped_shm_base;

	region = (struct axivc_region *)mapped_shm_base;
	axivc_endpoint_init(
		&vdev->endpoint, &region->publisher_to_subscriber,
		&region->subscriber_to_publisher);

	ret = misc_register(&vdev->misc);
	if (ret)
	{
		mutex_lock(&pub_vdev_lock);
		pub_vdev_count--;
		mutex_unlock(&pub_vdev_lock);
		kfree(vdev);
		goto unmap_region;
	}

	mutex_lock(&pub_vdev_lock);
	list_add_tail(&vdev->list, &pub_vdev_list_head);
	mutex_unlock(&pub_vdev_lock);

	snprintf(pub_dev_name, MAX_IVC_DEV_NAME_LENGTH, "/dev/%s", vdev->name);
	return 0;

unmap_region:
	memunmap(mapped_shm_base);
rollback_channel:
	axivc_rollback_publish(channel_key);
	return ret;
}

static int ivc_unpublish_channel(u64 channel_key)
{
	struct axivc_publisher_vdev *vdev;
	int ret;

	mutex_lock(&pub_vdev_lock);
	list_for_each_entry(vdev, &pub_vdev_list_head, list)
	{
		if (vdev->key != channel_key)
			continue;
		if (vdev->active || vdev->removing)
		{
			mutex_unlock(&pub_vdev_lock);
			return -EBUSY;
		}
		vdev->removing = true;
		mutex_unlock(&pub_vdev_lock);

		ret = hvc_unpublish_channel(channel_key);
		if (ret)
		{
			mutex_lock(&pub_vdev_lock);
			vdev->removing = false;
			mutex_unlock(&pub_vdev_lock);
			return -EIO;
		}

		misc_deregister(&vdev->misc);
		mutex_lock(&pub_vdev_lock);
		list_del(&vdev->list);
		pub_vdev_count--;
		mutex_unlock(&pub_vdev_lock);
		memunmap(vdev->mapped_shm_base);
		kfree(vdev);
		return 0;
	}
	mutex_unlock(&pub_vdev_lock);
	return -ENOENT;
}

static int ivc_subscribe_channel(u64 publisher_id, u64 key, char *sub_dev_name)
{
	u64 shm_base_ptr;
	u64 shm_size_ptr;
	struct axivc_subscriber_vdev *vdev;
	struct axivc_hvc_output *hvc_output;
	struct axivc_region *region;
	void *mapped_shm_base;
	unsigned long deadline;
	u64 shm_base;
	u64 shm_size;
	int id;
	int ret;

	hvc_output = kzalloc(sizeof(*hvc_output), GFP_KERNEL);
	if (!hvc_output)
		return -ENOMEM;
	shm_base_ptr = kva2pa((u64)&hvc_output->shm_base);
	shm_size_ptr = kva2pa((u64)&hvc_output->shm_size);
	if (shm_base_ptr == ~0ULL || shm_size_ptr == ~0ULL)
	{
		kfree(hvc_output);
		return -EFAULT;
	}

	ret = hvc_subscribe_channel(publisher_id, key, shm_base_ptr, shm_size_ptr);
	if (ret)
	{
		kfree(hvc_output);
		return -EIO;
	}
	shm_base = hvc_output->shm_base;
	shm_size = hvc_output->shm_size;
	kfree(hvc_output);

	/* This region participates in the same normal-memory atomic protocol as
	 * the Rust peer; it must not be accessed through an MMIO mapping. */
	mapped_shm_base = memremap(shm_base, shm_size, MEMREMAP_WB);
	if (!mapped_shm_base)
	{
		ret = -ENOMEM;
		goto rollback_subscription;
	}

	deadline = jiffies + msecs_to_jiffies(AXIVC_SUBSCRIBE_WAIT_MS);
	for (;;)
	{
		ret =
			axivc_region_validate(mapped_shm_base, shm_size, publisher_id, key);
		if (ret != -EAGAIN || time_after_eq(jiffies, deadline))
			break;
		if (msleep_interruptible(AXIVC_POLL_INTERVAL_MS))
		{
			ret = -ERESTARTSYS;
			break;
		}
	}
	if (ret)
		goto unmap_subscription;

	vdev = kzalloc(sizeof(*vdev), GFP_KERNEL);
	if (!vdev)
	{
		ret = -ENOMEM;
		goto unmap_subscription;
	}

	mutex_lock(&sub_vdev_lock);
	if (sub_vdev_count >= MAX_VDEVS)
	{
		mutex_unlock(&sub_vdev_lock);
		kfree(vdev);
		ret = -ENOSPC;
		goto unmap_subscription;
	}
	id = next_sub_vdev_id++;
	sub_vdev_count++;
	mutex_unlock(&sub_vdev_lock);
	snprintf(
		vdev->name, sizeof(vdev->name), "%s%d", IVC_SUBSCRIBER_DEV_NAME_PREFIX,
		id);
	vdev->misc.minor = MISC_DYNAMIC_MINOR;
	vdev->misc.name = vdev->name;
	vdev->misc.fops = &axivc_subscriber_fops;
	vdev->id = id;
	vdev->publisher_id = publisher_id;
	vdev->key = key;
	vdev->shm_base = shm_base;
	vdev->shm_size = shm_size;
	vdev->mapped_shm_base = mapped_shm_base;

	region = (struct axivc_region *)mapped_shm_base;
	axivc_endpoint_init(
		&vdev->endpoint, &region->subscriber_to_publisher,
		&region->publisher_to_subscriber);

	ret = misc_register(&vdev->misc);
	if (ret)
	{
		mutex_lock(&sub_vdev_lock);
		sub_vdev_count--;
		mutex_unlock(&sub_vdev_lock);
		kfree(vdev);
		goto unmap_subscription;
	}

	mutex_lock(&sub_vdev_lock);
	list_add_tail(&vdev->list, &sub_vdev_list_head);
	mutex_unlock(&sub_vdev_lock);

	snprintf(sub_dev_name, MAX_IVC_DEV_NAME_LENGTH, "/dev/%s", vdev->name);
	return 0;

unmap_subscription:
	memunmap(mapped_shm_base);
rollback_subscription:
	axivc_rollback_subscribe(publisher_id, key);
	return ret;
}

static int ivc_unsubscribe_channel(u64 publisher_id, u64 key)
{
	struct axivc_subscriber_vdev *vdev;
	int ret;

	mutex_lock(&sub_vdev_lock);
	list_for_each_entry(vdev, &sub_vdev_list_head, list)
	{
		if (vdev->publisher_id != publisher_id || vdev->key != key)
			continue;
		if (vdev->active || vdev->removing)
		{
			mutex_unlock(&sub_vdev_lock);
			return -EBUSY;
		}
		vdev->removing = true;
		mutex_unlock(&sub_vdev_lock);

		ret = hvc_unsubscribe_channel(publisher_id, key);
		if (ret)
		{
			mutex_lock(&sub_vdev_lock);
			vdev->removing = false;
			mutex_unlock(&sub_vdev_lock);
			return -EIO;
		}

		misc_deregister(&vdev->misc);
		mutex_lock(&sub_vdev_lock);
		list_del(&vdev->list);
		sub_vdev_count--;
		mutex_unlock(&sub_vdev_lock);
		memunmap(vdev->mapped_shm_base);
		kfree(vdev);
		return 0;
	}
	mutex_unlock(&sub_vdev_lock);
	return -ENOENT;
}

static void axivc_notify_publisher(bool enabled, u64 publisher_id, u64 key)
{
	u64 ret;

	if (!enabled)
		return;
	ret = hvc_notify_channel(publisher_id, key, publisher_id);
	if (ret)
		WARNING(
			"axvisor: IVC notify failed for publisher %llu key 0x%llx: "
			"%llu\n",
			publisher_id, key, ret);
}

static void axivc_abort_send(
	struct axivc_endpoint *endpoint, bool notify_publisher, u64 publisher_id,
	u64 key)
{
	struct axivc_message_sender *tx = &endpoint->tx;
	unsigned long deadline =
		jiffies + msecs_to_jiffies(AXIVC_TRANSFER_TIMEOUT_MS);
	bool published = tx->published_any;
	int ret;

	for (;;)
	{
		ret = axivc_message_try_abort(tx);
		if (ret != -EAGAIN)
			break;

		/* A full ring is the normal reason an in-flight send failed. Keep
		 * retrying so a peer which resumes can observe ABORT instead of being
		 * stranded forever in the partial logical message. */
		axivc_notify_publisher(notify_publisher, publisher_id, key);
		if (time_after_eq(jiffies, deadline))
			break;
		msleep(AXIVC_POLL_INTERVAL_MS);
	}

	if (!ret && published)
		axivc_notify_publisher(notify_publisher, publisher_id, key);
	else if (ret && ret != -ENOMSG)
		endpoint->tx_error = -EPIPE;
}

static ssize_t axivc_send_message(
	struct axivc_endpoint *endpoint, bool notify_publisher, u64 publisher_id,
	u64 key, const char __user *buf, size_t count)
{
	struct axivc_message_sender *tx = &endpoint->tx;
	unsigned long deadline;
	size_t sent = 0;
	int ret;

	/* POSIX read/write cannot distinguish an empty logical message from
	 * "no data". Keep the device ABI explicit instead of silently losing
	 * the Message V1 boundary. */
	if (!count)
		return -EOPNOTSUPP;

	ret = mutex_lock_interruptible(&endpoint->tx_lock);
	if (ret)
		return ret;
	if (endpoint->tx_error)
	{
		ret = endpoint->tx_error;
		goto unlock;
	}

	ret = axivc_message_start(tx, count);
	if (ret)
		goto unlock;

	deadline = jiffies + msecs_to_jiffies(AXIVC_TRANSFER_TIMEOUT_MS);
	for (;;)
	{
		u8 fragment[AXIVC_FRAGMENT_CAPACITY];
		struct axivc_send_progress progress;
		size_t chunk = min_t(size_t, sizeof(fragment), count - sent);

		if (chunk && copy_from_user(fragment, buf + sent, chunk))
		{
			ret = -EFAULT;
			goto abort;
		}
		ret = axivc_message_try_write(tx, fragment, chunk, &progress);
		if (ret)
			goto abort;

		sent += progress.consumed;
		if (progress.published_cells)
		{
			deadline = jiffies + msecs_to_jiffies(AXIVC_TRANSFER_TIMEOUT_MS);
			axivc_notify_publisher(notify_publisher, publisher_id, key);
		}
		if (progress.complete)
		{
			mutex_unlock(&endpoint->tx_lock);
			return count;
		}
		if (progress.published_cells)
			continue;
		if (time_after_eq(jiffies, deadline))
		{
			ret = -ETIMEDOUT;
			goto abort;
		}
		if (msleep_interruptible(AXIVC_POLL_INTERVAL_MS))
		{
			ret = -ERESTARTSYS;
			goto abort;
		}
	}

abort:
	axivc_abort_send(endpoint, notify_publisher, publisher_id, key);
unlock:
	mutex_unlock(&endpoint->tx_lock);
	return ret;
}

static ssize_t axivc_recv_message(
	struct axivc_endpoint *endpoint, bool notify_publisher, u64 publisher_id,
	u64 key, char __user *buf, size_t count)
{
	struct axivc_message_receiver *rx = &endpoint->rx;
	struct axivc_message_meta meta;
	unsigned long deadline;
	size_t received = 0;
	bool available;
	ssize_t ret;

	if (!count)
		return 0;
	ret = mutex_lock_interruptible(&endpoint->rx_lock);
	if (ret)
		return ret;

	ret = axivc_message_peek_meta(rx, &meta, &available);
	if (ret || !available)
		goto unlock;
	if (meta.len > count)
	{
		ret = -EMSGSIZE;
		goto unlock;
	}

	deadline = jiffies + msecs_to_jiffies(AXIVC_TRANSFER_TIMEOUT_MS);
	for (;;)
	{
		u8 fragment[AXIVC_FRAGMENT_CAPACITY];
		struct axivc_receive_progress progress;

		ret = axivc_message_try_read(rx, fragment, sizeof(fragment), &progress);
		if (ret)
		{
			/* A valid ABORT cell was consumed even though the message API
			 * reports it as an error; notify the producer that ring space was
			 * released. */
			if (ret == -ECONNRESET)
				axivc_notify_publisher(notify_publisher, publisher_id, key);
			goto unlock;
		}
		if (progress.consumed_cells)
		{
			deadline = jiffies + msecs_to_jiffies(AXIVC_TRANSFER_TIMEOUT_MS);
			axivc_notify_publisher(notify_publisher, publisher_id, key);
		}
		if (progress.written &&
			copy_to_user(buf + received, fragment, progress.written))
		{
			ret = -EFAULT;
			axivc_message_receiver_poison(rx, ret);
			goto unlock;
		}
		received += progress.written;
		if (progress.complete)
		{
			/* Consume a valid transport-level empty message so it cannot
			 * block the ring, but report that the read/write adapter cannot
			 * represent its boundary. */
			ret = meta.len == 0 ? -EOPNOTSUPP : received;
			goto unlock;
		}
		if (progress.consumed_cells)
			continue;
		if (time_after_eq(jiffies, deadline))
		{
			ret = -ETIMEDOUT;
			axivc_message_receiver_poison(rx, ret);
			goto unlock;
		}
		if (msleep_interruptible(AXIVC_POLL_INTERVAL_MS))
		{
			ret = -ERESTARTSYS;
			axivc_message_receiver_poison(rx, ret);
			goto unlock;
		}
	}

unlock:
	mutex_unlock(&endpoint->rx_lock);
	return ret;
}

static ssize_t axivc_publisher_read(
	struct file *file, char __user *buf, size_t count, loff_t *ppos)
{
	struct axivc_publisher_vdev *vdev = file->private_data;

	if (!vdev->active)
		return -ENODEV;
	return axivc_recv_message(&vdev->endpoint, false, 0, vdev->key, buf, count);
}

static ssize_t axivc_publisher_write(
	struct file *file, const char __user *buf, size_t count, loff_t *ppos)
{
	struct axivc_publisher_vdev *vdev = file->private_data;

	if (!vdev->active)
		return -ENODEV;
	return axivc_send_message(&vdev->endpoint, false, 0, vdev->key, buf, count);
}

static int axivc_publisher_open(struct inode *inode, struct file *file)
{
	struct axivc_publisher_vdev *vdev =
		container_of(file->private_data, struct axivc_publisher_vdev, misc);

	mutex_lock(&pub_vdev_lock);
	if (vdev->active || vdev->removing)
	{
		int ret = vdev->removing ? -ENODEV : -EBUSY;

		mutex_unlock(&pub_vdev_lock);
		return ret;
	}
	vdev->active = true;
	mutex_unlock(&pub_vdev_lock);
	file->private_data = vdev;
	return 0;
}

static int axivc_publisher_release(struct inode *inode, struct file *file)
{
	struct axivc_publisher_vdev *vdev = file->private_data;

	mutex_lock(&pub_vdev_lock);
	vdev->active = false;
	mutex_unlock(&pub_vdev_lock);
	return 0;
}

static ssize_t axivc_subscriber_read(
	struct file *file, char __user *buf, size_t count, loff_t *ppos)
{
	struct axivc_subscriber_vdev *vdev = file->private_data;

	if (!vdev->active)
		return -ENODEV;
	return axivc_recv_message(
		&vdev->endpoint, true, vdev->publisher_id, vdev->key, buf, count);
}

static ssize_t axivc_subscriber_write(
	struct file *file, const char __user *buf, size_t count, loff_t *ppos)
{
	struct axivc_subscriber_vdev *vdev = file->private_data;

	if (!vdev->active)
		return -ENODEV;
	return axivc_send_message(
		&vdev->endpoint, true, vdev->publisher_id, vdev->key, buf, count);
}

static int axivc_subscriber_open(struct inode *inode, struct file *file)
{
	struct axivc_subscriber_vdev *vdev =
		container_of(file->private_data, struct axivc_subscriber_vdev, misc);

	mutex_lock(&sub_vdev_lock);
	if (vdev->active || vdev->removing)
	{
		int ret = vdev->removing ? -ENODEV : -EBUSY;

		mutex_unlock(&sub_vdev_lock);
		return ret;
	}
	vdev->active = true;
	mutex_unlock(&sub_vdev_lock);
	file->private_data = vdev;
	return 0;
}

static int axivc_subscriber_release(struct inode *inode, struct file *file)
{
	struct axivc_subscriber_vdev *vdev = file->private_data;

	mutex_lock(&sub_vdev_lock);
	vdev->active = false;
	mutex_unlock(&sub_vdev_lock);
	return 0;
}

static long
axivc_manager_ioctl(struct file *file, unsigned int ioctl, unsigned long arg)
{
	int ret = 0;
	ivc_publish_arg_t publish_arg;
	ivc_subscribe_arg_t subscribe_arg;
	uint64_t shm_size = 0;

	switch (ioctl)
	{
	case IVC_PUBLISH_CHANNEL:
		if (copy_from_user(
				&publish_arg, (u64 __user *)arg, sizeof(ivc_publish_arg_t)))
		{
			ERROR("axvisor: Failed to copy channel key from user space\n");
			return -EFAULT;
		}

		INFO(
			"axvisor: Publishing channel with key: 0x%llx, size: 0x%llx\n",
			publish_arg.channel_key, publish_arg.channel_size);

		shm_size = publish_arg.channel_size;
		ret = ivc_publish_channel(
			publish_arg.channel_key, shm_size, publish_arg.device_name);
		if (ret)
			return ret;

		if (copy_to_user(
				(u64 __user *)arg, &publish_arg, sizeof(ivc_publish_arg_t)))
		{
			ivc_unpublish_channel(publish_arg.channel_key);
			return -EFAULT;
		}

		break;
	case IVC_UNPUBLISH_CHANNEL:
		if (copy_from_user(
				&publish_arg, (u64 __user *)arg, sizeof(ivc_publish_arg_t)))
		{
			ERROR("axvisor: Failed to copy channel key from user space\n");
			return -EFAULT;
		}

		INFO(
			"axvisor: Unpublishing channel with key: 0x%llx\n",
			publish_arg.channel_key);

		ret = ivc_unpublish_channel(publish_arg.channel_key);
		if (ret)
		{
			ERROR(
				"axvisor: Failed to unpublish channel with key: 0x%llx\n",
				publish_arg.channel_key);
			return ret;
		}

		break;
	case IVC_SUBSCRIBE_CHANNEL:
		if (copy_from_user(
				&subscribe_arg, (u64 __user *)arg, sizeof(ivc_subscribe_arg_t)))
		{
			ERROR("axvisor: Failed to copy channel key from user space\n");
			return -EFAULT;
		}

		INFO(
			"axvisor: Subscribing to channel [%llu] with key: 0x%llx\n",
			subscribe_arg.target_publisher_id, subscribe_arg.channel_key);

		ret = ivc_subscribe_channel(
			subscribe_arg.target_publisher_id, subscribe_arg.channel_key,
			subscribe_arg.device_name);
		if (ret)
		{
			ERROR(
				"axvisor: Failed to subscribe to channel %llu\n",
				subscribe_arg.target_publisher_id);
			return ret;
		}

		if (copy_to_user(
				(u64 __user *)arg, &subscribe_arg, sizeof(ivc_subscribe_arg_t)))
		{
			ivc_unsubscribe_channel(
				subscribe_arg.target_publisher_id, subscribe_arg.channel_key);
			return -EFAULT;
		}
		break;
	case IVC_UNSUBSCRIBE_CHANNEL:
		if (copy_from_user(
				&subscribe_arg, (u64 __user *)arg, sizeof(ivc_subscribe_arg_t)))
		{
			ERROR("axvisor: Failed to copy channel key from user space\n");
			return -EFAULT;
		}

		INFO(
			"axvisor: Unsubscribing from channel [%llu] with key: 0x%llx\n",
			subscribe_arg.target_publisher_id, subscribe_arg.channel_key);

		ret = ivc_unsubscribe_channel(
			subscribe_arg.target_publisher_id, subscribe_arg.channel_key);
		if (ret)
		{
			ERROR(
				"axvisor: Failed to unsubscribe from channel %llu\n",
				subscribe_arg.target_publisher_id);
			return ret;
		}

		break;
	default:
		ERROR("axvisor: Invalid ioctl command\n");
		return -EINVAL;
	}

	return ret;
}

static int axivc_manager_open(struct inode *inode, struct file *file)
{
	INFO("axvisor: Opened device %s success\n", IVC_DEV_NAME);
	return 0;
}

static int axivc_manager_release(struct inode *inode, struct file *file)
{
	int ret = 0;
	INFO("axvisor: Closing device %s\n", IVC_DEV_NAME);
	return ret;
}

static const struct file_operations axivc_publisher_fops = {
	.owner = THIS_MODULE,
	.open = axivc_publisher_open,
	.read = axivc_publisher_read,
	.write = axivc_publisher_write,
	.release = axivc_publisher_release,
};

static const struct file_operations axivc_subscriber_fops = {
	.owner = THIS_MODULE,
	.open = axivc_subscriber_open,
	.read = axivc_subscriber_read,
	.write = axivc_subscriber_write,
	.release = axivc_subscriber_release,
};

static const struct file_operations axivc_manager_fops = {
	.owner = THIS_MODULE,
	.open = axivc_manager_open,
	.unlocked_ioctl = axivc_manager_ioctl,
	.compat_ioctl = axivc_manager_ioctl,
	.release = axivc_manager_release,
};

static struct miscdevice axvisor_ivc_management_vdev = {
	.minor = MISC_DYNAMIC_MINOR,
	.name = IVC_DEV_NAME,
	.fops = &axivc_manager_fops,
};

int init_ivc_devices(void)
{
	int ret = 0;

	// Register a IVC management device.
	ret = misc_register(&axvisor_ivc_management_vdev);
	if (ret)
	{
		WARNING(
			"axvisor: Failed to register misc device %s\n",
			axvisor_ivc_management_vdev.name);
		return ret;
	}
	INFO(
		"axvisor: IVC management device registered with name %s\n",
		axvisor_ivc_management_vdev.name);

	return ret;
}

void uninit_ivc_devices(void)
{
	struct axivc_publisher_vdev *pvdev, *ptmp;
	struct axivc_subscriber_vdev *svdev, *stmp;

	list_for_each_entry_safe(pvdev, ptmp, &pub_vdev_list_head, list)
	{
		hvc_unpublish_channel(pvdev->key);
		misc_deregister(&pvdev->misc);
		list_del(&pvdev->list);
		memunmap(pvdev->mapped_shm_base);
		kfree(pvdev);
	}
	list_for_each_entry_safe(svdev, stmp, &sub_vdev_list_head, list)
	{
		hvc_unsubscribe_channel(svdev->publisher_id, svdev->key);
		misc_deregister(&svdev->misc);
		list_del(&svdev->list);
		memunmap(svdev->mapped_shm_base);
		kfree(svdev);
	}
	misc_deregister(&axvisor_ivc_management_vdev);
}
