#!/usr/bin/env ruby

require "date"
require "json"
require "optparse"

def mean(values)
  values.reduce(0, :+).to_f / values.size.to_f
end

def percentile(values, percentile, sorted: false)
  return values[0] if values.size == 1

  values = values.sort unless sorted
  k = (percentile * (values.length - 1) + 1).floor - 1
  f = (percentile * (values.length - 1) + 1).modulo(1)

  values[k] + (f * (values[k + 1] - values[k]))
end

def time_difference_in_us(begin_ts, end_ts)
  ((end_ts - begin_ts) * 24 * 3600 * 1_000_000).to_f
end

def format_percentage(v)
  format("%.02f%%", (v * 100))
end

def format_time(v)
  if v >= 1_000_000
    format("%.2fs", (v.to_f / 1_000_000))
  elsif v >= 1_000
    format("%.2fms", (v.to_f / 1_000))
  else
    format("%.2fus", v)
  end
end

class Block
  attr_reader :height
  attr_accessor :tx_list, :commit_ts, :mining_time, :verify_time, :propose_end_ts

  def initialize(height)
    @height = height
  end

  def keep?
    return false unless @commit_ts
    return false if @commit_ts <= $tx_send_start_ts
    return false if @commit_ts >= $tx_send_end_ts

    true
  end
end

class Tx
  attr_reader :id
  attr_accessor :block_height, :send_ts, :propose_recv_ts, :commit_ts, :exec_time, :exec_storage_node

  def initialize(id)
    @id = id
  end

  def set_outdated
    @outdated = true
  end

  def outdated?
    @outdated
  end

  def set_conflicted
    @conflicted = true
  end

  def conflicted?
    @conflicted
  end

  def committed?
    !@commit_ts.nil?
  end

  def keep?
    return false unless @send_ts
    return false if @send_ts <= $tx_send_start_ts
    return false if @send_ts >= $tx_send_end_ts
    return false if @commit_ts && @commit_ts >= $tx_send_end_ts

    committed? || outdated? || conflicted?
  end

  def propose_time
    @propose_time ||= begin
      end_ts = $blocks[@block_height]&.propose_end_ts
      time_difference_in_us(@propose_recv_ts, end_ts) if @propose_recv_ts && end_ts
    end
  end

  def blk_mining_time
    @mining_time ||= begin
      $blocks[@block_height].mining_time
    end
  end

  def blk_verify_time
    @verify_time ||= begin
      $blocks[@block_height].verify_time
    end
  end

  def latency
    @latency ||= begin
      time_difference_in_us(@send_ts, @commit_ts) if @send_ts && @commit_ts
    end
  end
end

$blocks = Hash.new { |hash, key| hash[key] = Block.new key }
$txs = Hash.new { |hash, key| hash[key] = Tx.new key }
$tx_send_start_ts = nil
$tx_send_end_ts = nil
$result = {}

def process_common(file, &block)
  File.readlines(file).map { |l| JSON.parse l }.each_with_index(&block)
end

def process_node_metrics!(file, client: false)
  process_common(file) do |data, line|
    case data["k"]
    when "event"
      case data["l"]
      when "client_event"
        next unless client

        case data["v"]["info"]
        when "send-tx-opts"
          puts "Opts used by send-tx:"
          pp data["v"]["data"]
          puts
        when "start-send-tx"
          $tx_send_start_ts = DateTime.iso8601 data["ts"]
        when "end-send-tx"
          $tx_send_end_ts = DateTime.iso8601 data["ts"]
          $result["send_tx_real_rate"] = data["v"]["data"]["real_rate"]
        else
          warn "Unknown client_event #{data["v"]["info"]} in #{file}:#{line}"
        end
      when "tx_begin"
        next unless client

        $txs[data["v"]["tx_id"]].send_ts = DateTime.iso8601 data["ts"]
      when "tx_commit"
        next unless client

        block = $blocks[data["v"]["height"]]
        block.commit_ts = DateTime.iso8601 data["ts"]
        block.tx_list = data["v"]["tx_ids"]
        block.tx_list.each do |tx_id|
          tx = $txs[tx_id]
          tx.block_height = block.height
          tx.commit_ts = block.commit_ts
        end
      when "blk_recv_tx"
        $txs[data["v"]["tx_id"]].propose_recv_ts = DateTime.iso8601 data["ts"]
      when "tx_outdated"
        $txs[data["v"]["tx_id"]].set_outdated
      when "tx_conflict"
        $txs[data["v"]["tx_id"]].set_conflicted
      when "propose_end"
        $blocks[data["v"]["height"]].propose_end_ts = DateTime.iso8601 data["ts"]
      else
        warn "Unknown event #{data["l"]} in #{file}:#{line}"
      end
    when "time"
      case data["l"]
      when "verify_block"
        $blocks[data["v"]["height"]].verify_time = data["t_in_us"]
      when "mining"
        $blocks[data["v"]["height"]].mining_time = data["t_in_us"]
      else
        warn "Unknown time record #{data["l"]} in #{file}:#{line}"
      end
    end
  end
end

def process_storage_node_metrics!(file, storage_node_id:)
  process_common(file) do |data, line|
    case data["k"]
    when "event"
      case data["l"]
      when "tx_commit"
      else
        warn "Unknown event #{data["l"]} in #{file}:#{line}"
      end
    when "time"
      case data["l"]
      when "exec_time"
        tx = $txs[data["v"]["tx_id"]]
        tx.exec_time = data["t_in_us"]
        tx.exec_storage_node = storage_node_id
      when "verify_block"
      else
        warn "Unknown time record #{data["l"]} in #{file}:#{line}"
      end
    end
  end
end

def post_process!
  old_blk_len = $blocks.size
  old_tx_len = $txs.size

  # puts "TX without state: #{$txs.count{ |_, tx| !tx.committed? && !tx.conflicted? && !tx.outdated? }}"

  $blocks.select! { |_, blk| blk.keep? }
  $txs.select! { |_, tx| tx.keep? }

  new_blk_len = $blocks.size
  new_tx_len = $txs.size

  puts "Ignore #{old_blk_len - new_blk_len} blocks. Remaining: #{new_blk_len}"
  puts "Ignore #{old_tx_len - new_tx_len} txs. Remaining: #{new_tx_len}"
  puts

  cal_success_rate!
  cal_tx_statistics!
  cal_block_statistics!
  cal_storage_node_statistics!
end

def cal_success_rate!
  total = $txs.size
  committed = $txs.count { |_, tx| tx.committed? }
  conflicted = $txs.count { |_, tx| tx.conflicted? }
  outdated = $txs.count { |_, tx| tx.outdated? }
  $result["total_tx"] = total
  $result["committed_tx"] = committed
  $result["conflicted_tx"] = conflicted
  $result["outdated_tx"] = outdated
  $result["committed_tx_percentage"] = committed.to_f / total.to_f
  $result["conflicted_tx_percentage"] = conflicted.to_f / total.to_f
  $result["outdated_tx_percentage"] = outdated.to_f / total.to_f
end

def cal_tx_statistics!
  committed_tx = $txs.select { |_, tx| tx.committed? }

  latency = committed_tx.map { |_, tx| tx.latency }.to_a.sort
  $result["avg_latency_in_us"] = mean(latency)
  $result["50percentile_latency_in_us"] = percentile(latency, 0.5, sorted: true)
  $result["90percentile_latency_in_us"] = percentile(latency, 0.9, sorted: true)
  $result["95percentile_latency_in_us"] = percentile(latency, 0.95, sorted: true)

  tx_exec_time = committed_tx.map { |_, tx| tx.exec_time || 0 }.to_a.sort
  $result["avg_tx_exec_time_in_us"] = mean(tx_exec_time)
  $result["50percentile_tx_exec_time_in_us"] = percentile(tx_exec_time, 0.5, sorted: true)
  $result["90percentile_tx_exec_time_in_us"] = percentile(tx_exec_time, 0.9, sorted: true)
  $result["95percentile_tx_exec_time_in_us"] = percentile(tx_exec_time, 0.95, sorted: true)

  tx_propose_time = committed_tx.map { |_, tx| tx.propose_time }.to_a.sort
  $result["avg_tx_blk_propose_time_in_us"] = mean(tx_propose_time)
  $result["50percentile_tx_blk_propose_time_in_us"] = percentile(tx_propose_time, 0.5, sorted: true)
  $result["90percentile_tx_blk_propose_time_in_us"] = percentile(tx_propose_time, 0.9, sorted: true)
  $result["95percentile_tx_blk_propose_time_in_us"] = percentile(tx_propose_time, 0.95, sorted: true)

  tx_mining_time = committed_tx.map { |_, tx| tx.blk_mining_time || 0 }.to_a.sort
  $result["avg_tx_blk_mining_time_in_us"] = mean(tx_mining_time)
  $result["50percentile_tx_blk_mining_time_in_us"] = percentile(tx_mining_time, 0.5, sorted: true)
  $result["90percentile_tx_blk_mining_time_in_us"] = percentile(tx_mining_time, 0.9, sorted: true)
  $result["95percentile_tx_blk_mining_time_in_us"] = percentile(tx_mining_time, 0.95, sorted: true)

  tx_verify_time = committed_tx.map { |_, tx| tx.blk_verify_time }.to_a.sort
  $result["avg_tx_blk_verify_time_in_us"] = mean(tx_verify_time)
  $result["50percentile_tx_blk_verify_time_in_us"] = percentile(tx_verify_time, 0.5, sorted: true)
  $result["90percentile_tx_blk_verify_time_in_us"] = percentile(tx_verify_time, 0.9, sorted: true)
  $result["95percentile_tx_blk_verify_time_in_us"] = percentile(tx_verify_time, 0.95, sorted: true)
end

def cal_block_statistics!
  tx_count = $blocks.map { |_, blk| blk.tx_list.size }.to_a.sort

  $result["total_block"] = $blocks.size
  $result["avg_tx_in_block"] = mean(tx_count)
  $result["50percentile_tx_in_block"] = percentile(tx_count, 0.5, sorted: true)
  $result["90percentile_tx_in_block"] = percentile(tx_count, 0.9, sorted: true)
  $result["95percentile_tx_in_block"] = percentile(tx_count, 0.95, sorted: true)

  blk_mining_time = $blocks.map { |_, blk| blk.mining_time }.to_a.sort
  $result["avg_blk_mining_time_in_us"] = mean(blk_mining_time)
  $result["50percentile_blk_mining_time_in_us"] = percentile(blk_mining_time, 0.5, sorted: true)
  $result["90percentile_blk_mining_time_in_us"] = percentile(blk_mining_time, 0.9, sorted: true)
  $result["95percentile_blk_mining_time_in_us"] = percentile(blk_mining_time, 0.95, sorted: true)

  blk_verify_time = $blocks.map { |_, blk| blk.verify_time }.to_a.sort
  $result["avg_blk_verify_time_in_us"] = mean(blk_verify_time)
  $result["50percentile_blk_verify_time_in_us"] = percentile(blk_verify_time, 0.5, sorted: true)
  $result["90percentile_blk_verify_time_in_us"] = percentile(blk_verify_time, 0.9, sorted: true)
  $result["95percentile_blk_verify_time_in_us"] = percentile(blk_verify_time, 0.95, sorted: true)

  total_commited_tx = $blocks.map { |_, blk| blk.tx_list.size }.reduce(0, :+)
  first_block = $blocks.min_by { |_, blk| blk.height }.last
  last_block = $blocks.max_by { |_, blk| blk.height }.last
  total_commited_tx -= first_block.tx_list.size
  duration = ((last_block.commit_ts - first_block.commit_ts) * 24 * 60 * 60).to_f
  $result["throughput"] = total_commited_tx.to_f / duration
end

def cal_storage_node_statistics!
  $txs.select { |_, tx| tx.exec_time }.group_by { |_, tx| tx.exec_storage_node }.each do |id, txs|
    $result["tx_exec_by_storage_node_#{id}"] = txs.size
  end
end

def report!(storage: true)
  puts <<~EOS
    # Sucess Rate
    total\tcommitted\tconflicted\toudated
    #{$result["total_tx"]}\t#{$result["committed_tx"]}\t#{$result["conflicted_tx"]}\t#{$result["outdated_tx"]}
    \t#{format_percentage $result["committed_tx_percentage"]}\t#{format_percentage $result["conflicted_tx_percentage"]}\t#{format_percentage $result["outdated_tx_percentage"]}

    # Tx Statistics
    total_tx: #{$result["total_tx"]}

    \tavg\t50th\t90th\t95th percentile
    latency\t#{format_time $result["avg_latency_in_us"]}\t#{format_time $result["50percentile_latency_in_us"]}\t#{format_time $result["90percentile_latency_in_us"]}\t#{format_time $result["95percentile_latency_in_us"]}
    exec\t#{format_time $result["avg_tx_exec_time_in_us"]}\t#{format_time $result["50percentile_tx_exec_time_in_us"]}\t#{format_time $result["90percentile_tx_exec_time_in_us"]}\t#{format_time $result["95percentile_tx_exec_time_in_us"]}
    propose\t#{format_time $result["avg_tx_blk_propose_time_in_us"]}\t#{format_time $result["50percentile_tx_blk_propose_time_in_us"]}\t#{format_time $result["90percentile_tx_blk_propose_time_in_us"]}\t#{format_time $result["95percentile_tx_blk_propose_time_in_us"]}
    mining\t#{format_time $result["avg_tx_blk_mining_time_in_us"]}\t#{format_time $result["50percentile_tx_blk_mining_time_in_us"]}\t#{format_time $result["90percentile_tx_blk_mining_time_in_us"]}\t#{format_time $result["95percentile_tx_blk_mining_time_in_us"]}
    verify\t#{format_time $result["avg_tx_blk_verify_time_in_us"]}\t#{format_time $result["50percentile_tx_blk_verify_time_in_us"]}\t#{format_time $result["90percentile_tx_blk_verify_time_in_us"]}\t#{format_time $result["95percentile_tx_blk_verify_time_in_us"]}

    # Block Statistics
    total_block: #{$result["total_block"]}
    throughput: #{$result["throughput"].round(2)} tx/s

    \tavg\t50th\t90th\t95th percentile
    #tx\t#{$result["avg_tx_in_block"]}\t#{$result["50percentile_tx_in_block"]}\t#{$result["90percentile_tx_in_block"]}\t#{$result["95percentile_tx_in_block"]}
    mining\t#{format_time $result["avg_blk_mining_time_in_us"]}\t#{format_time $result["50percentile_blk_mining_time_in_us"]}\t#{format_time $result["90percentile_blk_mining_time_in_us"]}\t#{format_time $result["95percentile_blk_mining_time_in_us"]}
    verify\t#{format_time $result["avg_blk_verify_time_in_us"]}\t#{format_time $result["50percentile_blk_verify_time_in_us"]}\t#{format_time $result["90percentile_blk_verify_time_in_us"]}\t#{format_time $result["95percentile_blk_verify_time_in_us"]}
  EOS

  return unless storage

  puts <<~EOS

    # Storage Node Statistics
    node\t#exec txs
  EOS

  (1...).each do |id|
    tx = $result["tx_exec_by_storage_node_#{id}"]
    break unless tx

    puts "#{id}\t#{tx}"
  end
end


if $PROGRAM_NAME == __FILE__
  options = {}
  opts = OptionParser.new do |opts|
    opts.banner = "Usage: #{$0} [options]"

    opts.on("-c", "--client FILE", "Client's metrics log file (required)") do |file|
      options[:client] = file
    end

    opts.on("-n", "--node FILE", "Other nodes' metrics log file") do |file|
      (options[:node] ||= []) << file
    end

    opts.on("-s", "--storage FILE", "Storage nodes' metrics log file") do |file|
      (options[:storage] ||= []) << file
    end

    opts.on("-o", "--output FILE", "Save result to json file") do |file|
      options[:output] = file
    end

    opts.on("-h", "--help") do
      puts opts
      exit
    end
  end
  opts.parse!

  unless options[:client]
    puts opts
    exit 1
  end

  process_node_metrics! options[:client], client: true
  options[:node].each do |f|
    process_node_metrics! f, client: false
  end if options[:node]
  options[:storage].each_with_index do |f, i|
    process_storage_node_metrics! f, storage_node_id: i + 1
  end if options[:storage]
  post_process!
  report!(storage: options[:storage]&.any?)

  File.write(options[:output], JSON.pretty_generate($result)) if options[:output]
end
